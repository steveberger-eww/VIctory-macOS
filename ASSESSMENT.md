# VIctory for macOS: engineering assessment, 25 November 2026

Not committed. It quotes observations from local game runs, so it must never
go into fleet posts, issues or pull requests (fleet rule 2). Delete or move it
before anything is pushed.

Written from the point of view of a senior Apple graphics and porting engineer
(Metal and AGX, D3DMetal and GPTK, Wine and Rosetta, Rust). It is based on a
read of all ~113k lines at `upstream/main` 1e6a5ac plus the uncommitted work on
`feat/sdl-controllers`, and on three recorded runs of the Series X build on an
M4 Max with 36 GB (D3DMetal 4.0 beta 3, CrossOver 26.3).

Each finding carries a file and line. "Verified" means I checked it against the
code or the built tree. "Inferred" means it is read from the code and still
needs a test or a profile to confirm.

## Summary

The project is well-built research code with excellent comments. Nearly every
shortcut is written down as a `shortcut:` note. The test discipline is real
(about 650 tests, a GPU differences suite, a test fleet). It gets a AAA title
into its world on a Mac, which few projects manage.

What holds it back is not one bug but the architecture of how the GPU reaches
title memory. Everything the title gives the GPU is copied into a separate
host copy before each submission and copied back afterwards. That happens
through a system call, under one global lock, with no overlap between CPU and
GPU. That single design explains most of the 0.3 to 1.7 fps, most of the 23 to
27 GB of wired memory, and indirectly the pool-overflow crash. On Apple Silicon
CPU and GPU share memory, so this copying exists only to solve addressing, not
to move data.

Second come the shader translator's gaps. In the world, 10 compute shaders
and 1 pixel shader are skipped every frame. They are most likely the
lighting, exposure and bloom passes, which matches the flat, washed-out
picture. Third, the Mac toolchain and the fleet have real robustness and
security holes.

### Top 12, by impact

| # | Severity | Finding | Layer |
|---|---|---|---|
| 1 | Critical | Per-submission bookkeeping scales with every copy, not with what changed: about 140,000 change checks over 69 GB of copies per frame, 197 ms in refresh batches and 40 ms in the guard handler. Moving the bytes (`ReadProcessMemory`) is only about 13 ms per frame, so an earlier guess that it was the cost is wrong (`copystat` log, `memory.rs:339-355`). | GPU |
| 2 | Critical | CPU and GPU never overlap. One global submission lock covers preparing copies, submitting, waiting for the GPU and copying back (`host.rs:880-1127`). Every title queue maps onto one host queue (`queue.rs:114-127`). | GPU |
| 3 | Critical | Memory leaks by design. Placed textures are never released (`resource.rs:1626`, verified). Windows outside buffers are never freed (`shadow.rs`). Each title-written texture is held up to 4 times. Wired memory reaches 23 to 27 GB, then macOS kills Metal's compiler service. | GPU |
| 4 | Critical | Mixed mask/data joins aren't supported: "a register joining a lane mask and data, read as data" (`victory-shader/src/emit/lower.rs:1790`). This alone rejects 6 of the 10 compute shaders skipped every frame. | Shader |
| 5 | Critical (inferred) | Wave64 on Apple's 32-lane SIMD groups. Programs declare `WaveSize(64)` (`lower.rs:534-537`), but nothing queries `WaveLaneCountMin/Max` (verified: no hits). If D3DMetal runs them 32 wide, thread ids, ballots and LDS races all break quietly. | Shader |
| 6 | Critical | Fleet volunteers run unsandboxed code from GitHub with their `gh` token in reach (`tools/fleet/fleet.sh:276`). The task commit isn't checked to be on main, and issue edits aren't checked (`lib.sh:22`, `fleet.sh:58`). | Fleet |
| 7 | High | Indirect execution supports only one-argument signatures and drops the call otherwise (`victory-gpu/src/list.rs:1388`, `draw.rs:1586`). Calls with a pipeline not ready after a 0.5 s budget are dropped (`pipeline/make.rs:10-19`). That fits "indirect kind 2: no host pipeline", 7 per frame. | GPU |
| 8 | High | Cross-queue and GPU memory waits do nothing (`queue.rs:166`, `list.rs:1453`), so async compute can read data that isn't ready. | GPU |
| 9 | High | Under the default Rosetta setup (memory not tracked), a raw-address copy-back writes whole 64 KiB pages over whatever the title wrote meanwhile (`shadow.rs:159-192`). Heap descriptors the title copies as bytes never reach the host (`descriptor.rs:177`, `681`). | GPU |
| 10 | High | The masked-store patch finds stores by scanning bytes, not by decoding instructions (`victory-loader/src/masked.rs:75`), and its trampolines have no unwind data (`masked.rs:413-475`). It is required on Rosetta, yet it can corrupt code or break SEH. | Loader |
| 11 | High | The crash path goes through Wine's `dbghelp!MiniDumpWriteDump`, which suspends every thread past VIctory's guard and crashes (seen in the 8.5-minute crash). | HLE |
| 12 | High | The Wine build hardcodes absolute paths under one home folder. Here `libgnutls` points at `/Users/steve/...`, which this account can't read (verified with `otool`). One cache alias is fought over by every checkout, including the fleet's (`setup.sh:49`). | Tooling |

## 1. GPU layer (`victory-gpu`, ~28k lines)

### Architecture verdict

The model is "title memory → host copy → GPU → copy back". It is careful and
well tested, but it is a desktop discrete-GPU design applied to a
unified-memory machine. Each submission does work in proportion to **all**
copies, not to what changed:
- `memory::prepare` adds every copy for any list that uses title memory (`memory.rs:339-355`).
- The page table is rebuilt over every page whenever any copy comes or goes: a ~32 MB owner vector and a 16.8 MB table write (`memory.rs:204-322`).
- Bindless refreshes every range any slot views (`bindless.rs:255`).

The run logs match this: 0.1 to 2.5 s of copying per frame against 0.3 to 1 s
of GPU time.

### Findings

- **Synchronous pipeline (critical).** No submission overlaps. Copy-back happens before `execute` returns. Fix: let 1 or 2 submissions run at once, snapshot per submission, and copy back when the fence completes.
- **System-call copies (critical, needs a benchmark).** `read()` and `write_mapped` use `ReadProcessMemory` and `WriteProcessMemory` so that a guarded page doesn't fault. With fault tracking already required on Rosetta, plain `memcpy` (falling back to the system call only on failure) should be far cheaper. A 256 MB timing test settles it.
- **Leaks (critical).**
  - Placed textures are never released, and replaced ones are kept (`resource.rs:1626-1646`). That is the 9 GB host-texture plateau.
  - Staging upload buffers persist per texture (`resource.rs:795`), and so does `last.read` (`texture.rs:314`).
  - The snapshot pool holds up to 2 GB (`shadow.rs:89`).
  - Each host view heap carries about 30 MB of upload tables (`descriptor/host.rs:131-140`).
- **Windows outside buffers (high, partly fixed locally).** Merging stopped the overlaps: 10.4 GB fell to 5.4 GB. Three gaps remain:
  - Windows are still never freed.
  - The 4 MiB minimum pads sparse use.
  - `find` checks only the 64 nearest buffers (`shadow.rs:832`), so a lookup can still miss a containing buffer and make a duplicate window. That needs an interval structure with a running maximum end.
- **Dropped work (high).**
  - Indirect calls with more than one argument are dropped (`list.rs:1388`, `draw.rs:1681-1690`).
  - Compute is dropped when the 0.5 s pipeline budget runs out. A dropped culling or bake dispatch leaves stale data for good, so compute should wait without a budget.
- **Synchronisation (high).** Queue `Wait` and `wait_value` do nothing (`queue.rs:166`, `list.rs:1453`).
- **Coherence under Rosetta (high).** Untracked copy-back writes whole pages (`shadow.rs:186`). `sync_heaps` skips untracked heaps (`descriptor.rs:177`).
- **Missing features (medium).**
  - `CreateReservedResource` and `GetResourceTiling`, though tiled resources tier 3 is advertised (`device.rs:250`).
  - Ray tracing, sampler feedback, predication and stream output.
  - Feature queries outside a few ids return `E_NOTIMPL`.
  - Each missing method is logged once (`object.rs:294-303`); use that list to set priorities.
- **Event wait (low, verified).** `wait_for` polls every 1000 ms when D3DMetal doesn't set the event (`host.rs:843`). It fired 0 times in the last run, so it is not today's slowdown, but it costs a whole second when it does.

### The Apple-native way out (the biggest win available)

- **Import title memory into Metal directly.** Test whether D3DMetal 4.0b3 implements `ID3D12Device3::OpenExistingHeapFromAddress`. It maps to `newBufferWithBytesNoCopy`: 16 KiB alignment, memory from `mmap`, which Wine's `VirtualAlloc` gives.
  - If it does: import each runtime GPU pool once, at `XMemVirtualAlloc`, and place title buffers in it.
  - That removes refreshes, snapshots, copy-back, the watch faults and windows outside buffers.
  - `memory.rs` already has the page table; it would point at imported heaps instead of copies.
- **If D3DMetal doesn't have it, reverse the direction.** Allocate the pools as CUSTOM / WRITE_BACK / L0 buffers (as `shared_buffer` already does, `host.rs:2925`) and hand the title those pointers. The only open question is placement below 2^40.
- **Textures:** detile on the GPU, reading straight from the imported memory.

Expected gain: an order of magnitude in frame time and most of the wired
memory. This is the change that turns a 1 fps demo into something playable.

## 2. Shader translator (`victory-shader`, ~43k lines)

### Architecture verdict

The design is good: SSA, then structurisation that removes EXEC, then range
analysis, resource tracking, and DXIL written by hand. Its limits are lane
masks used as data, descriptors that aren't plain loads, and Apple-specific
code size.

### Missing, matched to what is skipped every frame

- **Mixed mask/data joins (critical).**
  - "a register joining a lane mask and data, read as data" (`lower.rs:1790`), and "a lane-mask join of an untracked mask" (`lower.rs:10114`).
  - The fix: give a mixed join both a number form (a ballot on the mask edges) and a per-lane form (the lane's own bit on the data edges), and treat `s_mov` of an inline constant as a mask constant in `structure.rs`.
  - Compute first. Pixel, vertex and mesh shaders have the same problem as "a lane mask read as data" (`lower.rs:379` and others).
- **Descriptors (high).**
  - "read only as data" (`lower.rs:4683`), "neither loaded nor built" (`resource.rs:1133`), "chosen by control flow" (`lower.rs:5149-5182`).
  - The fix: lower joins recursively to a non-uniform heap index, with a fallback that reads the raw 8 dwords from guest memory (the `memory()` page lookup exists at `lower.rs:4562`) and maps guest descriptor addresses to heap indices.
- **Opcodes (medium).**
  - `v_cvt_f32_f64`: there is no f64 lowering at all, and Apple GPUs have no fp64, so emulate with integers.
  - `v_permlanex16_b32`: only inside the wave simulation (`emit/wave.rs:493`).
  - Barycentrics: `lower.rs:1410`, mapped to `SV_Barycentrics`; Metal supports them on Apple7 and later.
- **Stages (medium for this title, high in general).** Tessellation and the NGG LS/HS/ES/GS stages are rejected (`emit/mod.rs:396-405`). So are primitive, viewport and render-target exports, cube arrays and indirect jumps.

### Risks specific to Apple

- **Wave64 on 32 lanes (critical, inferred).** See the summary. First, a test that reads `WaveGetLaneCount()` under `WaveSize(64)` on D3DMetal. Then, if needed, a wave64-on-32 lowering: two host waves per console wave, with ballots joined through groupshared memory.
- **Apple's compiler crashing (high).** The `RegAllocBase` segfaults point at code size and register pressure:
  - A `v_movrel*` with no proven bound becomes up to 256 compare-and-select steps per instruction, keeping every candidate VGPR live (`lower.rs:5700-5744`).
  - The 64-bit atomic emulation loops over lanes with a nested spin and two barriers per atomic (`lower.rs:4299-4440`).
  - Every guarded instruction becomes its own `if`, and every loop gets a counter.
  - The DXIL validator never runs on the Mac (`validator.rs:1-3`).
  - Next step: dump the failing modules (`diagnostics`) and bisect them with `metal-shaderconverter` offline.
- **Mesh output over 32 KB (high).** Outputs are sized at the declared maxima and only counts are checked, never bytes (`lower.rs:2808-2821`). That is why the pipelines fail with 34,816 bytes. Fix: check bytes before building the pipeline, then pack f16 pairs (`half_outputs`, `lower.rs:446`) or drop outputs nothing reads.
- **`v_readfirstlane` in divergent loops (high).** D3DMetal returns other lanes' values there (`docs/macos.md`), and only descriptor waterfalls are handled (`lower.rs:6287`). Value loops can get wrong values, or spin up to `LOOP_LIMIT = 2^20`.
- **Float behaviour (medium).** The MODE register isn't modelled, there is no denormal mode, FMA becomes `FMad` (`lower.rs:5849`), and `v_rcp` becomes a true division.
- **Quad ops in compute (medium).** They assume the host's quads are lanes 4n..4n+3 (`lower.rs:2889`).
- **LDS (medium).** Addresses are assumed aligned and in bounds (`lower.rs:3074`).

### Quality

- **`emit/lower.rs` is 10,950 lines.** Its biggest functions run 480 to 740 lines, and instructions are dispatched by matching name strings. Split it by instruction family and dispatch on an opcode enum.
- **Translation cost is unbounded.** There is a boolean graph of up to 2^21 nodes with 256 fixed-point rounds (`structure.rs:146`), `range::build` reruns the pipeline up to 8 more times (`range.rs:52`), and resource tracking takes up to 64 rounds. One shader took 1.9 s. Needed: a time budget, translation off the render thread, and failures cached by hash.
- **Errors carry only a fixed string and an offset.** Add the instruction text, register and stage.
- **Tests.** All GPU-difference suites are Windows-only, so on the Mac they run only under Wine. Not tested: a real shader corpus, fuzzing, mesh output size, wave size on the host, f64, and compiling with Apple's own compiler.

## 3. HLE, loader, PE and core (~30k lines)

### Architecture verdict

Tidy modules, the best comments in the repo, and sensible host pass-through.
The weak spots are where the console's timing and memory model meet Wine on
Rosetta.

### Findings

- **Crash path (high).** Override `dbghelp!MiniDumpWriteDump` to return FALSE, or write a dump without memory. The vectored handler also allocates and writes to stderr on every first-chance fault (`crash.rs:596-740`), which can deadlock on the heap lock.
- **Masked-store patch (high).**
  - Hits are found by byte scan (`masked.rs:75`). Validate each one by decoding linearly from the `.pdata` function starts, as `tls.rs` does.
  - Register unwind data for the trampolines (`RtlAddFunctionTable`).
  - Only the main image is patched; title DLLs aren't (`apps/victory/src/run.rs:88-91`).
  - Segment prefixes are dropped, and the trampoline pool silently stops at 1 MiB.
- **xmem locking (high).** The `PHYSICAL` mutex is held across `VirtualFree`, `MapViewOfFile3` and `UnmapViewOfFile2`, each of which goes through the wineserver (`xmem.rs:745-918`). DirectStorage workers wait on the same mutex, so streaming stalls behind tiled-heap remapping.
- **Physical page numbers are never reused (medium).** The title packs them into 21 bits (`xmem.rs:548-566`), so they wrap after about 124 GiB of total churn.
- **The title's own `VirtualFree` bypasses the bookkeeping (medium)**, and reported memory use is the whole Wine process (`xmem.rs:1073`), which inflates the title's budget view.
- **DirectStorage (high).**
  - The fence signal (slot 0x28) is missing, and so are cancel and the error slots (`dstorage.rs:1055`).
  - The page-0 heuristic can't tell a reused page list from an unfilled one; snapshot the list at submit time instead.
  - The new 200 ms wait (uncommitted) can hold a worker and part of the 32 MB staging budget.
  - 16 workers is too many next to the title's own threads under Rosetta.
- **Async and threads (high).**
  - `XTaskQueueTerminate` ignores `wait` (`xasync.rs:393-418`), which risks use-after-free.
  - Pool threads grow without bound and never exit (`xasync.rs:168, 237`). Under Wine each start and exit also runs every image's TLS callbacks.
  - Threads, affinity, priorities and `Sleep` go straight to the host. The title sees 16 cores instead of the console's 7, and the header comment claiming they are overridden is wrong (`host.rs:9-10`).
- **I/O under fault tracking (high).** Overlapped `ReadFile` becomes synchronous (`files.rs:552`). `ReadFileEx`, `NtReadFile`, `recv` and completion-port writes into guarded memory aren't covered, and under Rosetta those can hang the process.
- **Async pairs that never complete (medium).** Begin/result pairs that return `E_NOTIMPL` without completing the async block (`xuser.rs`, `xgameui.rs`, `xstore.rs`) can hang a title that waits on it.
- **Audio (medium).** SHAPE has no rate conversion, EQ or mixing. XMA decodes from cold state with no overlap, so seams may be audible. Reverb convolution outputs zeros.
- **The INSTANCE_LIST crash (inferred).** Clock-paced producers (streaming, XMA, timers) run at full speed while per-frame cleanup runs at 1 fps. The real fix is frame time (section 1). Until then, a diagnostic counter of pool-related activity over time would confirm it.

## 4. macOS stack, tooling, fleet and process

### Findings

- **Fleet security (critical).**
  - Tasks run as the volunteer's own user, with `gh auth token` readable.
  - Every push to main becomes tasks (`fleet-rounds.yml`).
  - Task commits aren't checked to be on main, and edits to bot issues aren't checked (`lib.sh:22`, `queue.sh:50`, `fleet.sh:58`).
  - Anyone can post schema-valid fake results.
  - Workflows with `issues: write` use actions that aren't pinned to SHAs.
  - Fix: a dedicated unprivileged macOS account without credentials, `merge-base --is-ancestor` against main, an editor check, and a separate step that only posts the checked JSON.
- **Asset-policy guard under Wine (critical).** Data and cache default to `C:\users\…`, which physically sits in `target/wineprefix-play/drive_c` inside the repo. Seen from `C:`, the guard never finds `.victory-root` (`victory-core/src/policy.rs:41-62`). A side effect: `cargo clean` deletes saves and the hour-long Wine build.
- **Wine build portability (high, verified).** Absolute install names under one home folder. `libgnutls.30.dylib` points at `/Users/steve/...` and fails to load for this account, so TLS inside Wine is broken. That doesn't matter offline, but it is a silent failure. Fix: `@rpath` or `@loader_path`, and a separate cache folder per checkout.
- **The D3DMetal version is never checked (high).** The docs require 4.0b3. `D3DMetal.framework/Resources/Info.plist` has `CFBundleShortVersionString`, and nothing reads it. The fleet guesses the version from the `.dmg` file name and picks the file differently from `setup.sh`.
- **GPU tests pass with no GPU (high).** About 13 tests `return` when `host()` is `None`. A broken D3DMetal counts as a pass. Fix: `VICTORY_REQUIRE_GPU=1` in `test-wine.sh`.
- **No macOS CI (high).** CI is `windows-latest` only. The gnullvm target, `tools/macos/*` and `setup.sh` are never built or linted, and Mac clippy runs skip all `cfg(windows)` code.
- **Script robustness (medium).**
  - Downloads have no `.part` file, and extraction is skipped once the folder exists.
  - The Wine source checksum is skipped for any version other than 26.3.0.
  - `setup.sh` doesn't detach images on failure, and a missing `d3d11.dll` reads as "D3DMetal already added".
  - The winebus SDL patch matches an exact line including its indentation. It fails loudly, which is good, but there is no patch file and no check that SDL actually loads (`WINEDEBUG=+hid`).
- **Known flaky tests run unquarantined (medium).** `root_views_*` and `heap_views_*` create noise findings in the fleet.
- **No memory budget (medium).** No `QueryVideoMemoryInfo` or residency handling, though 16 GB Macs already run out.
- **Missing for a production Mac pipeline:**
  - signing and notarization
  - an app bundle
  - a DXVK/MoltenVK fallback (Wine is built `--without-vulkan`)
  - crash symbolication
  - profiling hooks (`xctrace` Metal System Trace, `MTL_HUD_ENABLED`)
  - a tool to manage D3DMetal's per-exe shader cache (entries aren't keyed by D3DMetal version)
- **Docs out of step with the code.**
  - Rust 1.100 against 1.98.1.
  - `windows-gnu` against gnullvm.
  - Test counts 343 against 481.
  - 15 against 25 GB free.
  - `CONTRIBUTING.md` says to post full test output, against fleet rule 3.
  - About 29 `VICTORY_*` switches, with no single reference.
  - `victory-pipeline` is unused.

## 5. Code quality across the repo

- **Strengths.** Comments that explain why. Shortcuts written down (49 in `victory-gpu` alone). Safety comments on most `unsafe`. Small focused crates. Strong unit coverage of tricky logic (page tables, window merge, refresh). The run-time diagnostics are unusually good: per-frame copy stats, black-frame detection, named missing methods.
- **Weaknesses.**
  - Very large files: `lower.rs` 10,950 lines, `host.rs` 3,309, `draw.rs` 2,506, `resource.rs` 2,478.
  - Heavy global state: 46 static mutexes in `victory-gpu`, with an implicit lock order (submission → GUARDS → per-copy), and title threads take GUARDS from the fault handler.
  - All 168 lock sites ignore poisoning.
  - About 1,200 `unsafe` blocks in GPU and HLE, some resting on a "single submitter" assumption that async submission would break (`shadow.rs:91-95`, `686-690`).
  - Host failures are often logged once, and then the work is silently dropped.
- **Test strategy gap.** Native `cargo test` on the Mac says little about the code that matters. GPU and HLE behaviour is exercised only under Wine, there are no concurrency or leak tests, and there is no corpus of real shaders.

## 6. Recommended plan

**Phase 0, a week.** Cheap fixes with large effect.
1. Benchmark `ReadProcessMemory` against `memcpy`. If confirmed, switch to `memcpy` with a fallback.
2. Release placed textures and stale staging buffers. Free unused windows. Find buffers with an interval structure.
3. Override `MiniDumpWriteDump`.
4. Wave-size probe test on D3DMetal.
5. Make GPU tests fail when there is no device. Check the D3DMetal version from `Info.plist`.
6. Fleet: an ancestry check, an editor check, and a credential-free runner account.

**Phase 1, 2 to 4 weeks.** The picture.
1. Mixed mask/data joins (6 of the 10 compute shaders skipped every frame).
2. Mesh output byte check and packing.
3. Descriptor fallback through guest memory.
4. Barycentrics, `v_permlanex16`, f64 conversions.
5. Multi-argument indirect, and no dropped compute.
6. Queue and memory waits.

**Phase 2, 1 to 2 months.** Performance architecture.
1. Asynchronous submission: 1 or 2 in flight, copy-back at fence completion.
2. A spike on importing title memory directly (`OpenExistingHeapFromAddress` / `newBufferWithBytesNoCopy`), then migrate the copy model.
3. GPU detiling straight from title memory.

**Phase 3, ongoing.** Robustness.
1. Validate the masked-store patch by decoding, and add unwind data.
2. Restructure xmem locking.
3. Bound the thread pools.
4. macOS CI on the gnullvm target.
5. `@rpath` for the Wine build.
6. Profiling hooks with `xctrace`.
7. Split `lower.rs` and `host.rs`.

## Evidence from the local runs (do not publish)

| Metric | Value |
|---|---|
| Frame rate in the world | 0.3 to 1.7 fps |
| Copies per frame | 100 to 500 MB |
| Host textures | about 9 GB |
| Windows outside buffers | 10.4 GB before the local merge, 5.4 GB after |
| Wired memory | 23 to 27 GB |
| Apple compiler failures per run | 241 to 350 (`RegAllocBase` segfaults); a warm cache cut crash reports from 36 to 8 |
| Mesh pipelines over Apple's 32 KB limit | 34,816 bytes |
| Skipped every frame | 10 compute shaders, 1 pixel shader, 7 indirect calls |
| DirectStorage page-list failures | 14 before the local fix, 0 after |
| Crash at 8.5 min | the title's own assert, with INSTANCE_LIST 45000/45000; then a crash in Wine's `dbghelp` |
