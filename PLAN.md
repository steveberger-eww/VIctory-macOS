# Plan: a correct picture, with shadows, on macOS

Not committed. It builds on `ASSESSMENT.md` and quotes local game runs, so it
must never go into fleet posts, issues or pull requests. Delete or move it
before anything is pushed.

Goal: the Series X build runs without crashing, every shader the frame uses
translates and compiles, lighting and shadows draw, and the picture is correct.
Speed matters only as far as making the work possible. Playable speed is a
separate track (section 8).

## 1. Where we are (25 November, M4 Max, D3DMetal 4.0b3)

- **The world draws, about 4,900 draws per frame, but unlit and washed out.** Surfaces are flat colours, people are see-through and white, and there are no shadows.
- **Skipped every frame:** 10 compute shaders and 1 pixel shader that VIctory can't translate, plus 7 indirect calls with no host pipeline.
- **Apple's compiler fails 241 to 350 times per run,** and mesh pipelines go over the 32 KB limit.
- **0.3 to 1.7 fps, with wired memory at 23 to 27 GB.** The game crashes after about 8.5 minutes, on its own full-pool assert.

The likely chain from what is skipped to what you see:

```
untranslated compute (mask/data joins, descriptors)
  → tile classification / light culling / deferred lighting don't run → flat, unlit surfaces
  → shadow-mask resolve doesn't run                                  → no shadows
  → luminance histogram / auto-exposure don't run                    → white washout
  → bloom / TAA inputs missing                                       → glow, ghosting
```

This is inferred. Step 0.2 replaces it with the real pass map.

## 2. Ground rules for the work

- **Measure first, every time.** Each step closes with a recorded run (`tools/macos/record.sh`, then `analyze-run.sh`) and, where it matters, a captured world frame.
- **Prefer offline loops.** Translating and compiling the shader pack takes minutes without the game; a game run takes 10 or more. Run the game only to confirm.
- **Every fix carries a test.** Translator fixes get a synthetic shader test in `victory-shader` (run under Wine). GPU fixes get a `victory-gpu` test.
- **Keep game material local.** Frames, dumps, logs and shader text never go into PRs or the fleet. PRs carry code and synthetic tests only.

## 3. Phase 0: instruments (3 to 5 days)

Nothing gets fixed blind. Build the dashboards first.

### 0.1 Census of every shader, offline

- **Translation census.** Run `victory translate --cache` over the title's shader pack (containers and SGD5 files). Record every shader's outcome: translated, or unsupported with its reason, plus the time it took.
  - Output: a table of reasons with counts and shader hashes, marked with which hashes the world frame actually uses (from the run log's "dropped … without a host pipeline").
- **Apple compile census.** Hand every translated DXIL to Apple's toolchain offline, using Metal Shader Converter (`metal-shaderconverter`) and `metal`. Record crashes, errors and compile time per shader.
  - This turns the 241 to 350 `MTLCompilerService` crashes into a reproducible list, without running the game.
- **New tool:** `tools/macos/shader-census.sh`, which writes a summary like `analyze-run.sh` does.

Exit: two tables, translation failures by reason and Apple compile failures by shader. Both can be rerun in minutes.

### 0.2 Pass map of one world frame

- One diagnostics run with `VICTORY_CAPTURE=<dir> VICTORY_CAPTURE_WORLD=60 VICTORY_DUMP_TARGETS=1`. That captures the 60th world frame and dumps every render target, with the pipeline that wrote it in the file name.
- From the dumps, write `passmap.md` (local): the frame's passes in order, with each pass's pipelines and inputs and outputs. Mark each pass as running, dropped (with the reason), or running but wrong.
- Find these passes explicitly:
  - **Shadow maps.** Depth-only draws into an atlas or cascades (texture array).
  - **Ray-traced shadows.** Compute that uses `image_bvh*`; the BVH itself comes from the title's builder dispatches.
  - **Screen-space and contact shadows,** and the shadow-mask resolve.
  - **Lighting.** Tile classification, light culling, deferred lighting.
  - **Exposure** (histogram, average), **bloom, TAA, upscaler** (FidelityFX), **tonemap, UI plane**.
- Use `VICTORY_DUMP_WRITERS=<hash>` and `VICTORY_TRACE_SHADERS=<hash>` on single shaders to confirm what each pass reads and writes.

Exit: every skipped shader is tied to the pass it belongs to. That gives the real priority order for phase 2.

### 0.3 A second reference

- **The best reference is a Windows PC with a real D3D12 GPU** running the same build. It separates "VIctory is wrong" from "D3DMetal is wrong". Per the README, Windows is unlit too today, so it doesn't give a correct picture. It does tell you whether a fault reproduces off the Mac.
- **Without one:** check suspect shaders under `VICTORY_D3D12_WARP` (the software adapter), if it works under Wine, and with the GPU differences test suites, which compare against RDNA2 hardware results.

### 0.4 Probes for questions only a device can answer (one test each, about a day)

**Results (25 November, M4 Max, D3DMetal 4.0b3):**
- **Wave size: critical.** The device reports 32 lanes only, accepts `WaveSize(64)` pipelines and runs nothing in them, silently. Every console wave64 program that needs 64 lanes was a no-op. Such programs are now refused under Wine and logged as "a wave64 program on a 32-lane host", so the next run's `analyze-run.sh` counts them. Phase 3.1 (wave64-on-32 lowering) moves up into phase 2, ordered by that count.
- **Direct import: unavailable.** `OpenExistingHeapFromAddress` returns `E_NOTIMPL`. Section 9's step 3 is closed until Apple implements it. Its fallback (CUSTOM pools handed to the title) does not fit physical-page mapping at title-chosen addresses either. The speed track has to come from cheaper bookkeeping and asynchronous submission.
- **Copy cost: not the problem.** The moves take about 13 ms per frame (copy stats); see 1.3.

- **Wave size.** Under `[WaveSize(64)]`, does D3DMetal run 64 lanes, refuse the pipeline, or quietly run 32? Read `WaveGetLaneCount()` and a ballot across all lanes.
- **Direct import of title memory.** Does D3DMetal 4.0b3 implement `ID3D12Device3::OpenExistingHeapFromAddress`? This decides section 8.
- **Copy cost.** Time `ReadProcessMemory` against `memcpy` for 256 MB under this Wine.

These answers change the plan, so they come first.

## 4. Phase 1: stable enough to work on (1 week)

Runs need to last longer than the debugging they serve.

| Step | Change | Where | Exit check |
|---|---|---|---|
| 1.1 | Release placed textures and replaced copies, and drop staging buffers once uploaded | `victory-gpu/src/resource.rs:795, 1626-1646`; `texture.rs:314` | Host textures fall when the title frees memory, not just plateau |
| 1.2 | Free windows outside buffers that no list or table has used for N frames; find buffers with an interval structure, so a containing buffer is never missed | `shadow.rs` (`loose`, `find`) | Windows under 2 GB after 20 min in the world |
| 1.3 | Dropped: the copy stats show the moves take about 13 ms per frame. The cost is the bookkeeping over every copy (197 ms in refresh batches), which belongs to the speed track (section 9) | – | – |
| 1.4 | `dbghelp!MiniDumpWriteDump` returns FALSE (HLE override); the crash handler stops allocating | `victory-hle`, `victory-loader/src/crash.rs` | A title assert ends with VIctory's report, not a crash in `dbghelp` |
| 1.5 | A clock-independent safeguard for the pool overflow: find the pool that fills up (INSTANCE_LIST), and as a stop-gap, pace streaming to frames | `dstorage.rs` pacing, investigation | 30 min in the world without the INSTANCE_LIST assert |
| 1.6 | `record.sh`: one folder per run even within the same second; captures go to the run folder | `tools/macos` | – |

Exit: a 30-minute session in the world with no crash, wired memory under 16
GB, and the frame rate no worse.

**Result (25 November, run 20261125-171049):**

| Exit target | Result |
| --- | --- |
| 30 minutes in the world without a crash | **Met.** 42 minutes, no pool assert, a steady 1.0 fps. |
| Wired memory under 16 GB | **Missed.** 22 GB peak. |

Why memory can't be met in this phase:
- Windows outside buffers fell to 3.7 GB and stay flat. Only 32 MB of that duplicates memory buffers already copy, so `find`'s lookup limit isn't a problem.
- Placed textures are 269 (352 MB), none of them stale.
- So nothing large is leaking. The 22 GB is the title's own working set kept twice: title memory plus host copies (about 9 GB of textures, 1.1 GB of buffers, 3.7 GB of windows). Only importing title memory directly (section 9) removes that.

1.1 and 1.2 therefore change from "free leaks" to "nothing to free". Their diagnostics stay.

New finding: closing the window crashes on the way out. Execution jumps to a non-canonical address and the whole stack is garbage. Low priority; look at the shutdown path (lifecycle, thread exit with TLS callbacks) when convenient.

## 5. Phase 2: every pass runs (2 to 4 weeks)

### 2.0 Wave64 on 32-lane waves (first; from probe 0.4)

The offline census of the whole shader pack (26,177 programs, `VICTORY_WAVE32_HOST=1 victory translate <game dir>`) counts 414 compute programs refused as wave64 on a 32-lane host.

| Group | Programs | Status |
| --- | --- | --- |
| LDS shared as one wave, no other lanes read | 120 | **74 run now** (`LdsLockstep`: barriers after each LDS access, hoisted to the end of per-lane branches). 46 remain: lanes leave the program inside a per-lane branch. |
| Lane reads, all at points every lane reaches | 98 | Open |
| Lane reads inside per-lane branches | 196 | Open |

What the lane reads are (census, per refused program):
- **Swizzles, DPP and `v_permlanex16` stay inside 32-lane halves** on RDNA2. Programs using only those (about 23) need console lane numbering on 32-lane waves and no exchange: console lane *l* is host wave *l/32*, lane *l%32*. That works in per-lane branches too. **Next step.**
- **`v_readlane`, `v_mbcnt` over per-lane masks, and masks used as numbers** need the other half. At points every lane reaches: exchange through groupshared memory with barriers, reusing `divergence`. About 75 programs.
- **The same inside per-lane branches** (about 196) needs another technique, for example one host thread running both console lanes *l* and *l+32*. Measure which of them the world frame runs before choosing.
- **The 46 early-exit LDS programs** need early exits turned into "skip to the end" (structuriser).

Order each by how often the world frame runs it. The run log's "does not translate" lines now carry these reasons.

Ordered by phase 0's pass map. The expected order follows.

### 2.1 Mixed lane-mask/data joins (most of the skipped compute)

- **Where it fails:** `victory-shader/src/emit/lower.rs:1790` ("a register joining a lane mask and data, read as data") and `lower.rs:10114` ("a lane-mask join of an untracked mask").
- **The fix in structurisation and lowering:**
  - Give a join whose inputs mix masks and data both forms: a number (on mask edges, the ballot of the lanes' booleans) and a per-lane boolean (on data edges, the lane's own bit of the value).
  - Each use picks the form it needs.
  - Treat `s_mov` of an inline constant as a mask constant (`structure.rs`).
- **Then the same for pixel, vertex and mesh** ("a lane mask read as data", `lower.rs:379, 5638, 6224, 7592`). These need console lane numbers. Mesh shaders get them from the thread id; pixel and vertex shaders stay limited.
- **Tests:** synthetic programs that join `vcc` from a compare with an `s_mov` constant and with `v_readfirstlane` results, checked against the RDNA2 results the suites already use.
- **Exit:** the 6 compute shaders and `a53a7977…` translate.

### 2.2 Descriptors

Covers "read only as data" (`lower.rs:4683`), "neither loaded nor built" (`resource.rs:1133`) and "chosen by control flow" (`lower.rs:5149`).

- Lower joins of descriptor sources recursively to a non-uniform heap index.
- Fallback: read the raw 8 dwords from guest memory through the page table (`memory()`, `lower.rs:4562`), with a GPU-readable map from guest descriptor addresses to heap indices.
- Prerequisite in `victory-gpu`: heap descriptors the title copies as bytes must reach the host even when memory isn't tracked (`descriptor.rs:177, 681`).
- Exit: `aab1b3c7…` and `39f0e9ae…` translate. The census shows the descriptor category gone.

### 2.3 Remaining opcodes and inputs

- **Barycentrics** (`lower.rs:1410`), which is pixel shader `98bd9f9a…`, skipped 3 times per frame. Use `SV_Barycentrics` with the matching interpolation, checked against the provoking vertex.
- **`v_permlanex16_b32`** in compute, through `WaveReadLaneAt` (it needs console lanes).
- **`v_cvt_f32_f64`** and the related f64 operations, emulated with integers. Apple GPUs have no fp64, and D3DMetal's doubles aren't trustworthy.

### 2.4 Apple's compiler crashes and the mesh limit

- **Triage.** Group the census's crashing shaders by what they emit. The suspects are `v_movrel*` compare chains (`lower.rs:5700-5744`), the 64-bit atomic emulation loops (`lower.rs:4299-4440`), huge guarded bodies and loop counters.
- **Fixes by group:**
  - `v_movrel*` without a proven bound: an indexable array (`alloca`-style local array) instead of 256 selects.
  - Atomics: share one loop across consecutive atomics.
  - Guards: merge runs of guarded instructions into one `if`.
  - If a crash group remains, find the smallest pattern that triggers it and file it with Apple (Feedback Assistant), with a synthetic shader only.
- **Mesh output size.** Compute the byte size before building the pipeline. Over 32 KB: pack f16 pairs (`half_outputs`, `lower.rs:446`), drop outputs nothing reads, then refuse with a clear message.
- **Exit:** the Apple compile census shows 0 crashes, and no mesh pipeline over the limit.

### 2.5 Nothing dropped in `victory-gpu`

- **Indirect calls with multi-argument signatures:** root constants, vertex and index buffer views, and root CBVs before the draw or dispatch (`list.rs:1388`, `draw.rs:1586, 1681`). This is the likely "indirect kind 2".
- **Compute is never dropped for a missing pipeline.** Wait for the pipeline without the 0.5 s budget (`pipeline/make.rs:10-19`); draws can keep the budget.
- **Exit:** the run log shows no "dropped … without a host pipeline" lines in the world.

Exit for phase 2: in the world, 0 shaders untranslated, 0 pipelines missing, 0
indirect calls dropped, 0 Apple compiler crashes.

## 6. Phase 3: every pass is correct (2 to 3 weeks)

Running isn't the same as correct. These are the faults that give wrong pixels
without any warning in the log.

| Step | Risk | Change | Check |
|---|---|---|---|
| 3.1 | Wave64 on 32 lanes (if probe 0.4 shows it) | Lower console wave64 as two host waves per console wave, with ballots and cross-lane reads through groupshared memory; until then refuse it, never run it quietly | Wave64 suite cases under Wine; light culling and histogram match |
| 3.2 | `v_readfirstlane` in divergent loops (a D3DMetal quirk, `docs/macos.md`) | Generalise the waterfall collapse to value loops (per-light, per-material), or read the value through groupshared memory | A synthetic per-light loop test |
| 3.3 | Copy-back overwrites title writes when memory isn't tracked | Raw-address copy-back by diff only (as the declared ranges already do), never whole pages | Existing `title_writes_under_a_busy_range…` plus an untracked variant |
| 3.4 | Cross-queue and memory waits do nothing | Implement `Wait` and `wait_value` on the host queue (fence waits, splitting submissions where needed) | Async compute test: producer on another queue, consumer reads |
| 3.5 | Float modes | Model MODE for denormals per shader; keep `v_fma_f32` as true FMA | VALU suite under Wine |
| 3.6 | Quad ops and derivatives in compute | Declare and check the quad layout D3DMetal uses | Quad-op suite case |
| 3.7 | LDS edge cases | Bounds-checked LDS access where the console would drop or zero it | Synthetic test |

Exit: in the pass map, every pass's outputs are plausible: not black, not NaN,
no tile-pattern garbage. The differences suites pass under Wine.

## 7. Phase 4: lighting and shadows (2 to 3 weeks, overlapping phase 3)

What the pass map from 0.2 shows decides the order. Each item below is a known
risk area for shadows on this stack, with how to check it.

### 4.1 Lighting first

Shadows only show through lighting. Once 2.1 and 2.2 land, recapture the pass
map and confirm in order:
1. tile classification
2. light culling (wave ops and ballots, which depend on 3.1)
3. deferred lighting
4. the shadow-mask inputs it reads

### 4.2 Shadow maps (rasterised)

- **Depth formats.** Apple GPUs have no D24, so D3DMetal keeps 32-bit float and rounds when it stores (`docs/macos.md`). Check:
  - that shadow passes keep the title's own depth bias, with D24's integer bias units converted to float depth (`DepthBias` × 2^-24 against D32F's exponent-based units);
  - slope-scaled bias and its clamp;
  - that the pipeline state carries them (`victory-gpu/src/pipeline/host.rs`).
  - Wrong bias shows as acne or Peter-panning, wholesale self-shadowing, or no shadow at all.
- **Comparison sampling.** `image_sample_c*` and `image_gather4_c*`, with the comparison function, border colour, PCF filtering and depth reference clamping. Add a synthetic test per variant.
- **Cascades and atlases.** Texture arrays and slices, and viewport and scissor per cascade. Render-target-index exports are refused today (assessment §2): if the title renders cascades through those (mesh or vertex `SV_RenderTargetArrayIndex`), that must be added.
- **Console depth layout and HTILE.**
  - If the title reads its depth or shadow maps as raw memory or typed views of other formats (common for resolving, downsampling or tile classification), the shared host texture must hand over the console's view of those bytes.
  - Check whether any shadow-pass consumer reads HTILE metadata directly. If one does, emulate HTILE: say "every tile uncompressed, full depth range".
  - Check sharing between `castable` host textures for depth formats.
- **Depth bounds** are dropped (`pipeline/host.rs:108`). They only cost speed, unless a pass relies on them to reject pixels (some shadow-mask passes do). Emulate with a discard in the pixel shader if the pass map shows one.

### 4.3 Ray-traced shadows (`xboxbvhbuilder.dll` and `image_bvh*`)

- **The BVH must exist.**
  - Every dispatch of the title's BVH builder has to run: no dropped pipeline, no untranslated shader. Check with `VICTORY_DUMP_WRITERS` on the builder's hashes.
  - BVH nodes have to land in a copy the ray shaders read. The builder writes through raw addresses, so that is the page table plus the copy-back path (3.3 matters here).
- **Traversal.**
  - `image_bvh_intersect_ray` is emulated in software (`victory-shader/src/emit/lower/bvh.rs`, with BVH tests).
  - Refused today: `a16` directions, instance and procedural nodes (they return misses), and descriptors with box growth or node bounds.
  - Check in the census which of these the title's shaders use. Instance nodes in particular matter for TLAS/BLAS scenes, and they would make every ray miss, so no RT shadows.
- **Speed.** Software traversal costs ALU per box and triangle test. Measure with `VICTORY_GPU_TIMING=1`. If the RT shadow pass dominates, the fallback is a lower ray count (if the title has a setting for it) or reusing the rasterised maps.

### 4.4 Screen-space and contact shadows

These usually use LDS, wave ops and depth reads, so they depend on 3.1, 3.2 and 3.7, and on the depth layout above.

Exit for phase 4: in a captured world frame, the shadow maps or BVH hold the
scene, the shadow mask has plausible penumbras, and lighting applies it.
Visually: shadows under the character and from buildings, stable from frame to
frame.

## 8. Phase 5: a correct final image (1 to 2 weeks)

- **Exposure.** The histogram and average-luminance compute (atomics, wave ops) must run, otherwise exposure sits at its start value and the picture washes out. Check the exposure buffer in the dump: it should be a finite value that changes over time.
- **Bloom and lens.** Downsample chains: check mips, the bias-as-gradient path in compute (`VICTORY_BIAS_AS_GRADIENTS`), and clamping.
- **TAA and the upscaler.**
  - Motion vectors, jitter, and history buffers kept across frames, so copies must not be dropped or stale.
  - The upscaler: the title's console FidelityFX build (`amd_fidelityfx_loader_xbox.dll`), or the PC FidelityFX DLL through `fidelityfx_dir` (`victory.toml`). Try both and keep the one that matches the reference.
- **Colour output.** sRGB format views, HDR off (SDR), and the present plane. The game presents two planes on Series consoles: overlay and picture. Test `VICTORY_PRESENT_COMPOSITE=1` once the overlay is confirmed transparent where the picture should show.
- **Golden frames.**
  - Keep captured world frames of a fixed scene (same save, same camera) as local references.
  - Compare after every change: a small script computing per-channel histograms or SSIM against the last good frame.
  - A change that moves the image without moving the census or the pass map is a regression to explain.

Exit: the fixed scene looks right next to the Windows or console reference.
Golden frames stay stable across five runs.

## 9. Parallel track: speed that makes the work bearable, then playable

Not required for a correct picture, but 1 fps makes every check slow.

1. **Phase 1.3 (`memcpy`) and the leak fixes.** Expect a large share of the copy time gone.
2. **Asynchronous submission.** 1 or 2 submissions in flight, snapshots per submission, copy-back when the fence completes (`host.rs:880-1127`). Use several host queues if the title's async compute matters.
3. **Import title memory directly** (if probe 0.4 succeeds).
   - Import each GPU pool once with `OpenExistingHeapFromAddress`, which maps to `newBufferWithBytesNoCopy`. Place title buffers there, and point the page table at those heaps.
   - That removes refreshes, snapshots, copy-back and windows outside buffers.
   - If D3DMetal lacks the call, reverse the direction: allocate the pools as CUSTOM/WRITE_BACK/L0 and give the title those pointers (it needs addresses below 2^40).
4. **GPU detiling** straight from title memory, instead of staging buffers.
5. **Profiling** with `xctrace record --template 'Metal System Trace'` wrapped by `record.sh`, and `MTL_HUD_ENABLED=1` for a quick live view.

Target: 15 fps or more in the world on an M4 Max, then 30.

## 10. Order and timeline

| Weeks | Main track | Parallel |
|---|---|---|
| 1 | Phase 0: census, pass map, probes | 1.3 `memcpy` (if the probe confirms) |
| 2 | Phase 1: stability | Phase 2.1 mask/data joins starts |
| 3–5 | Phase 2: every pass runs | 2.4 Apple compiler triage |
| 5–7 | Phase 3: every pass correct | Asynchronous submission |
| 6–8 | Phase 4: lighting and shadows | Memory-import spike |
| 8–9 | Phase 5: final image, golden frames | Memory import |

These estimates are for one engineer, and they depend on what phase 0 turns up.
The wave-size probe and the pass map can each move phases 3 and 4 by weeks.

## 11. Numbers to track on every run (`analyze-run.sh`)

| Number | Today | Phase 2 exit | Final |
|---|---|---|---|
| Untranslated shaders in the world, per frame | 11 | 0 | 0 |
| Indirect calls dropped per frame | 7 | 0 | 0 |
| Apple compiler failures per run | 241–350 | 0 crashes | 0 |
| Mesh pipelines over the limit | yes | 0 | 0 |
| Wired memory, peak | 23–27 GB | < 16 GB | < 12 GB |
| Minutes in the world without a crash | 8.5 | 30 | ∞ |
| Frame rate in the world | 0.3–1.7 | – | 15+ |
| Golden-frame difference to reference | n/a | – | within tolerance |

Extend `analyze-run.sh` to print these as one table, so each run is judged in
one look.
