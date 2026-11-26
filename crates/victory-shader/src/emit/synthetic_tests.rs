//! Synthetic GFX10 programs of what the 2020 compiler does not produce,
//! lowered to DXIL, validated by `dxil.dll` when present, and run on the
//! host GPU and WARP. Gathers at a level (`image_gather4_l`, `_l_o`), which
//! DXIL lacks: at level 0 the host's own gather (`image_gather4_lz`) of the
//! same coordinates is the reference on the hardware adapter (RDNA2, as the
//! console): it rounds the exact product of coordinate and size to 1/256 of
//! a texel, half up, which WARP does not. Lane permutes (`ds_bpermute_b32`).

use super::host::{Dispatch, Host, Root, Stride, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

/// Per lane (32, wave32): the record [offset word, u, v, level] at entry 1
/// of the table at s0 (a 2D texture at entry 0, a raw UAV at entry 2); the
/// level converted from an integer (so it is whole). Gathers red at the
/// level, with the offset, and at level 0 (`lz`) into 12 dwords at lane × 48.
fn program(whole: bool) -> Vec<u32> {
    let mut w = vec![
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF40C_0302,
        0xFA00_0000, // s_load_dwordx8 s[12:19], s[4:5], 0x0
        0xF408_0502,
        0xFA00_0020, // s_load_dwordx4 s[20:23], s[4:5], 0x20
        0xF408_0A02,
        0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
        // The culling shaders' sampler: border, transparent black, levels
        // 0-12, linear filters.
        0xB008_01B6, // s_movk_i32 s8, 0x1b6
        0xBE89_03FF,
        0x00C0_0000, // s_mov_b32 s9, 0xc00000
        0xBE8A_03FF,
        0x1850_0000, // s_mov_b32 s10, 0x18500000
        0xBE8B_0380, // s_mov_b32 s11, 0
        0x3420_0084, // v_lshlrev_b32 v16, 4, v0
        0x1628_00B0, // v_mul_u32_u24 v20, 48, v0
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xE038_1000,
        0x8005_0C10, // buffer_load_dwordx4 v[12:15], v16, s[20:23], 0 offen
        0xBF8C_3F70, // s_waitcnt vmcnt(0)
    ];
    if whole {
        w.push(0x7E1E_0D0F); // v_cvt_f32_u32 v15, v15
    }
    w.extend([
        0xF110_0108,
        0x0043_040D, // image_gather4_l v[4:7], v[13:15], s[12:19], s[8:11]
        0xF150_0108,
        0x0043_080C, // image_gather4_l_o v[8:11], v[12:15], s[12:19], s[8:11]
        0xF11C_0108,
        0x0043_100D, // image_gather4_lz v[16:19], v[13:14], s[12:19], s[8:11]
        0xBF8C_3F70, // s_waitcnt vmcnt(0)
        0xE078_1000,
        0x800A_0414, // buffer_store_dwordx4 v[4:7], v20, s[40:43], 0 offen
        0xE078_1010,
        0x800A_0814, // buffer_store_dwordx4 v[8:11], v20, s[40:43], 0 offen offset:16
        0xE078_1020,
        0x800A_1014, // buffer_store_dwordx4 v[16:19], v20, s[40:43], 0 offen offset:32
        0xBF81_0000, // s_endpgm
    ]);
    w
}

fn build(words: &[u32]) -> Result<HostShader, crate::ShaderError> {
    let range = |kind, table_offset_dwords, dwords, stride| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride,
        dwords,
        user_data: 0,
        slot: 0,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 32,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![
            range(ResourceType::Srv, 0, 8, 0),
            range(ResourceType::Srv, 8, 4, 16),
            range(ResourceType::Uav, 16, 4, 0),
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 1,
            threads: [32, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let lowered = lowered(&native)?;
    Ok(HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        static_samplers: lowered
            .samplers
            .iter()
            .map(|&w| super::StaticSampler::from_words(w))
            .collect(),
        samplers: lowered.samplers,
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        memory: lowered.memory,
        memory_writes: lowered.memory_writes,
        view_addresses: lowered.view_addresses,
        heap: lowered.heap,
        heap_writes: lowered.heap_writes,
        view_marks: lowered.view_marks,
    })
}

fn lowered(native: &NativeProgram) -> Result<lower::Lowered, crate::ShaderError> {
    lowered_on(native, false)
}

/// [`lowered`] for a host whose waves are 32 lanes or not.
fn lowered_on(native: &NativeProgram, wave32_host: bool) -> Result<lower::Lowered, crate::ShaderError> {
    lowered_halves(native, wave32_host, false)
}

/// [`lowered_on`], with wave64 programs run as two 32-lane halves or not.
fn lowered_halves(
    native: &NativeProgram,
    wave32_host: bool,
    wave64_halves: bool,
) -> Result<lower::Lowered, crate::ShaderError> {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, native).expect("io::raise");
    lower::lower(&lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        atomic_locks: false,
        bias_as_gradients: false,
        wave32_host,
        wave64_halves,
        half_io: false,
        view_marks: false,
    })
}

/// The first texel of a footprint (see `Lower::gather_first`).
fn first(u: f32, size: u32) -> i64 {
    let u = if u.is_nan() { 0.0 } else { u.clamp(-2.0, 2.0) };
    ((f64::from(u) * f64::from(size) * 512.0).floor() as i64 - 255) >> 9
}

#[test]
fn gathers_at_a_level_match_the_hardware() {
    // 24×16 then 12×8, 6×4, 3×2; red = level × 1000 + y × 32 + x + 1.
    let mut texels = Vec::new();
    for l in 0..4 {
        let (w, h) = (24 >> l, 16 >> l);
        for y in 0..h {
            for x in 0..w {
                texels.push([(l * 1000 + y * 32 + x + 1) as f32, 0.0, 0.0, 1.0]);
            }
        }
    }
    let texel = |l: u32, x: i64, y: i64| -> f32 {
        let (w, h) = (24i64 >> l, 16i64 >> l);
        let skip: i64 = (0..l).map(|k| (24i64 >> k) * (16 >> k)).sum();
        if (0..w).contains(&x) && (0..h).contains(&y) {
            texels[(skip + y * w + x) as usize][0]
        } else {
            0.0
        }
    };
    // Records: lanes 0-15 at level 0 where rounding decides (coordinates
    // whose f32 product with 24 lands on the other side of a texel's
    // threshold than the exact one, ties of 1/512 on the 16 texels of y,
    // NaN, infinities, zeros); lanes 16-31 at other levels and offsets.
    let mut records: Vec<[u32; 4]> = Vec::new();
    let rounded = |u: f32| ((f64::from(u * 24.0) * 512.0).floor() as i64 - 255) >> 9;
    let mut straddling = (1..24).filter_map(|i| {
        let u0 = ((f64::from(i) + 127.5 / 256.0) / 24.0) as f32;
        (-1024i32..1024)
            .map(|d| f32::from_bits(u0.to_bits().wrapping_add_signed(d)))
            .find(|&u| rounded(u) != first(u, 24))
    });
    for k in 0..16 {
        let tie = (f64::from(k - 3) + 0.5 - 1.0 / 512.0) / 16.0;
        let u = match k {
            0 => f32::NAN,
            1 => f32::INFINITY,
            2 => f32::NEG_INFINITY,
            3 => -0.0,
            _ => straddling.next().unwrap(),
        };
        records.push([0, u.to_bits(), (tie as f32).to_bits(), 0]);
    }
    for k in 0..16u32 {
        let level = [1, 2, 3, 7][k as usize % 4];
        let offset = [0x0000, 0x3F01, 0x0302, 0x3E3D][k as usize / 4];
        let (u, v) = (
            (k as f32 * 0.77 + 0.3) / 13.0 - 0.1,
            (k as f32 * 0.41 + 0.2) / 9.0,
        );
        records.push([offset, u.to_bits(), v.to_bits(), level]);
    }
    let input: Vec<u32> = records.iter().flatten().copied().collect();
    let shader = build(&program(true)).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .expect("DXIL validation failed");
    }
    let views = [
        View::Mipped(24, 16, 4, &texels),
        View::Buffer {
            data: &input,
            stride: Stride::Structured(16),
        },
        View::RawUav(32 * 12),
    ];
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host.compute_pipeline(&shader).expect("pipeline");
        let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &views,
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
        ran += 1;
        // Lanes 0-15: the host's own gather, ours, and the host's own as it
        // would be with u × 24 taken in f32.
        let mut own = Vec::new();
        for (lane, r) in records.iter().enumerate() {
            let got = |k: usize| -> Vec<f32> {
                out[0][lane * 12 + 4 * k..][..4]
                    .iter()
                    .map(|&b| f32::from_bits(b))
                    .collect()
            };
            let [offset, u, v, level] = *r;
            let level = level.min(3);
            let (w, h) = (24 >> level, 16 >> level);
            let at = |ox: i64, oy: i64| {
                let (x, y) = (
                    first(f32::from_bits(u), w) + ox,
                    first(f32::from_bits(v), h) + oy,
                );
                vec![
                    texel(level, x, y + 1),
                    texel(level, x + 1, y + 1),
                    texel(level, x + 1, y),
                    texel(level, x, y),
                ]
            };
            let sext = |b: u32| i64::from((b as i32) << 26 >> 26);
            assert_eq!(got(0), at(0, 0), "{label}: lane {lane}");
            assert_eq!(
                got(1),
                at(sext(offset), sext(offset >> 8)),
                "{label}: lane {lane}, offset"
            );
            if lane < 16 && !warp {
                let u = f32::from_bits(u);
                let x = if u.is_finite() {
                    rounded(u)
                } else {
                    first(u, 24)
                };
                let y = first(f32::from_bits(v), 16);
                let in_f32 = vec![
                    texel(0, x, y + 1),
                    texel(0, x + 1, y + 1),
                    texel(0, x + 1, y),
                    texel(0, x, y),
                ];
                own.push((lane, got(2), got(0), in_f32));
            }
        }
        // WARP's own gather truncates where the hardware rounds. D3DMetal's
        // rounds as the hardware does, but from u × 24 taken in f32, so on
        // the lanes where that product and the exact one fall on either
        // side of a texel's threshold it starts a texel over (docs/macos.md,
        // "Open differences"). Ours follows the console's hardware, so such
        // a host is not compared with it.
        let in_f32 = own.iter().any(|(_, host, ours, _)| host != ours)
            && own.iter().all(|(_, host, _, in_f32)| host == in_f32);
        if in_f32 {
            eprintln!("{label}: the host's own gather takes u × 24 in f32; not compared with ours");
        } else {
            for (lane, host, ours, _) in &own {
                assert_eq!(host, ours, "{label}: lane {lane} against its gather");
            }
        }
    }
    assert!(ran > 0, "no device");
}

#[test]
fn gathers_at_a_level_that_may_be_a_fraction_stop() {
    let Err(crate::ShaderError::Unsupported { what, .. }) = build(&program(false)) else {
        panic!("translated a gather at a level read from memory");
    };
    assert_eq!(what, "a gather at a level that may not be whole");
}

#[test]
fn permutes_read_the_lane_named_and_zero_from_lanes_left_out() {
    // Lanes below 24 run the permute; each reads lane (13 × lane + 5) mod 32
    // (its address in dwords, offset 20 bytes) of 1000 + lane.
    let shader = build(&[
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0A02,
        0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
        0x1602_00B4, // v_mul_u32_u24 v1, 52, v0
        0x4A04_00FF,
        0x0000_03E8, // v_add_nc_u32 v2, 1000, v0
        0x7E06_02FF,
        0x0000_DEAD, // v_mov_b32 v3, 0xdead
        0x3420_0082, // v_lshlrev_b32 v16, 2, v0
        0x7D88_0098, // v_cmp_gt_u32 vcc_lo, 24, v0
        0xBE94_3C6A, // s_and_saveexec_b32 s20, vcc_lo
        0xDACC_0014,
        0x0300_0201, // ds_bpermute_b32 v3, v1, v2 offset:20
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xBEFE_0314, // s_mov_b32 exec_lo, s20
        0xE070_1000,
        0x800A_0310, // buffer_store_dword v3, v16, s[40:43], 0 offen
        0xBF81_0000, // s_endpgm
    ])
    .expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .expect("DXIL validation failed");
    }
    let want: Vec<u32> = (0..32)
        .map(|l| match (13 * l + 5) % 32 {
            _ if l >= 24 => 0xDEAD,
            src if src < 24 => 1000 + src,
            _ => 0,
        })
        .collect();
    let views = [
        View::Texture(1, 1, &[[0.0; 4]]),
        View::Buffer {
            data: &[0; 4],
            stride: Stride::Structured(16),
        },
        View::RawUav(32),
    ];
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        // The program declares the console's wave of 32, which WARP refuses.
        let Ok((root, pso)) = host.compute_pipeline(&shader) else {
            assert!(warp, "{label}: no pipeline for a wave of 32");
            eprintln!("{label}: no pipeline for a wave of 32; skipped");
            continue;
        };
        let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &views,
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
        assert_eq!(out[0], want, "{label}");
    }
}

#[test]
fn ordered_pixel_sections_use_rasterizer_ordered_views() {
    // A pixel program's ordered section (POPS) as the title's compiler
    // writes it: the collision id after the primitive mask (s10, user data
    // ending at s9), a wait loop on the exiting wave id, a read-modify-write
    // of a UAV, the end of the section.
    let code: Vec<u32> = vec![
        0xBE84_0308, // s_mov_b32 s4, s8
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF40C_0302,
        0xFA00_0000, // s_load_dwordx8 s[12:19], s[4:5], 0x0
        0x9000_9F0A, // s_lshr_b32 s0, s10, 31
        0xBF06_8000, // s_cmp_eq_u32 s0, 0
        0x8580_807E, // s_cselect_b64 s[0:1], exec, 0
        0x8102_81EF, // s_add_i32 s2, src_pops_exiting_wave_id, 1
        0xBF08_8502, // s_cmp_gt_u32 s2, 5
        0x8582_807E, // s_cselect_b64 s[2:3], exec, 0
        0x8880_0200, // s_or_b64 s[0:1], s[0:1], s[2:3]
        0x87EA_007E, // s_and_b64 vcc, exec, s[0:1]
        0xBF87_0004, // s_cbranch_vccnz +4
        0xBF8E_0001, // s_sleep 1
        0x8100_81EF, // s_add_i32 s0, src_pops_exiting_wave_id, 1
        0xBF0A_8500, // s_cmp_lt_u32 s0, 5
        0xBF84_FFFC, // s_cbranch_scc0 -4
        0x7E00_0280, // v_mov_b32 v0, 0
        0x7E02_0280, // v_mov_b32 v1, 0
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xF000_3F88,
        0x0003_0200, // image_load v[2:5], v[0:1], s[12:19] dmask:0xf dim:2D unorm dlc glc
        0xBF8C_3F70, // s_waitcnt vmcnt(0)
        0x0604_04F2, // v_add_f32 v2, 1.0, v2
        0xF020_1F08,
        0x0003_0200, // image_store v[2:5], v[0:1], s[12:19] dmask:0xf dim:2D unorm
        0xBF90_0007, // s_sendmsg sendmsg(MSG_ORDERED_PS_DONE)
        0xF800_1890,
        0x0000_0000, // exp null off, off, off, off done vm
        0xBF81_0000, // s_endpgm
    ];
    let native = |db_shader_control| NativeProgram {
        stage: HwStage::Ps,
        wave64: true,
        vgprs: 8,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 8,
            slot: 0,
        }],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 8,
            slot: 0,
        }],
        io: StageIo::Ps {
            inputs: Vec::new(),
            input_addr: 0,
            db_shader_control,
            patches: Vec::new(),
        },
        code: code.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let l = lowered(&native(1 << 16)).expect("lower");
    assert!(l.rovs);
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut l.dxil.clone())
            .expect("DXIL validation failed");
    }
    // Without the ordered mode the collision id is no value the host has.
    let Err(crate::ShaderError::Unsupported { what, .. }) = lowered(&native(0)) else {
        panic!("translated a collision id outside an ordered section");
    };
    assert_eq!(what, "a hardware SGPR input");
}

/// A wave64 compute program on the host's wave size. Per thread (64, one
/// console wave) it writes its console lane index (`v_mbcnt` over all
/// lanes), the active lanes (`s_bcnt1 exec`) and its thread id, at thread
/// id × 16. On the console, lane = thread id and 64 lanes are active. The
/// translator declares `WaveSize(64)` for such a program. D3DMetal (4.0
/// beta 3, M4 Max) reports 32 lanes, accepts that pipeline and runs nothing
/// in it: every record stayed zero, thread ids too, while the same program
/// as wave32 ran as the console. On such a host the translation must refuse
/// the program ([`super::Options::wave32_host`]) rather than vanish.
#[test]
fn a_wave64_program_sees_64_lanes() {
    // Wave32 first, as a control of the program and the harness: lanes 0-31
    // twice, 32 active.
    let control: Vec<[u32; 3]> = (0..64).map(|t| [t % 32, 32, t]).collect();
    if let Some(got) = wave_probe(false) {
        assert_eq!(got, control, "wave32 control");
    }
    if let Some(got) = wave_probe(true) {
        let want: Vec<[u32; 3]> = (0..64).map(|t| [t, 64, t]).collect();
        assert_eq!(got, want, "the host does not run console wave64 as 64 lanes");
    }
}

/// Lanes per wave the host runs at most (0 when it does not say).
fn host_lanes(host: &Host) -> u32 {
    use windows::Win32::Graphics::Direct3D12::*;
    let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
    // SAFETY: the feature's own struct and size.
    let ok = unsafe {
        host.device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS1,
            (&raw mut o).cast(),
            std::mem::size_of_val(&o) as u32,
        )
    };
    if ok.is_ok() { o.WaveLaneCountMax } else { 0 }
}

/// Runs the probe as wave64 or wave32 (None: no device).
fn wave_probe(wave64: bool) -> Option<Vec<[u32; 3]>> {
    let code: Vec<u32> = vec![
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0A02,
        0xFA00_0000, // s_load_dwordx4 s[40:43], s[4:5], 0x0
        0xD765_0001,
        0x0001_00C1, // v_mbcnt_lo_u32_b32 v1, -1, 0
        0xD766_0001,
        0x0002_02C1, // v_mbcnt_hi_u32_b32 v1, -1, v1
        // s_bcnt1_i32_b64 s6, exec (wave32: s_bcnt1_i32_b32 s6, exec_lo)
        if wave64 { 0xBE86_107E } else { 0xBE86_0F7E },
        0x7E04_0206, // v_mov_b32 v2, s6
        0x7E06_0300, // v_mov_b32 v3, v0
        0x3408_0084, // v_lshlrev_b32 v4, 4, v0
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xE07C_1000,
        0x800A_0104, // buffer_store_dwordx3 v[1:3], v4, s[40:43], 0 offen
        0xBF81_0000, // s_endpgm
    ];
    let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    let names: Vec<String> = crate::gfx10::decode(&bytes)
        .expect("decode")
        .iter()
        .map(|i| i.name().unwrap_or("?").to_string())
        .collect();
    assert_eq!(
        names,
        [
            "s_mov_b32",
            "s_mov_b32",
            "s_load_dwordx4",
            "v_mbcnt_lo_u32_b32",
            "v_mbcnt_hi_u32_b32",
            if wave64 { "s_bcnt1_i32_b64" } else { "s_bcnt1_i32_b32" },
            "v_mov_b32",
            "v_mov_b32",
            "v_lshlrev_b32",
            "s_waitcnt",
            "buffer_store_dwordx3",
            "s_endpgm"
        ],
        "the hand-encoded words"
    );
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 8,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 1,
            threads: [64, 1, 1],
        }),
        code: bytes,
    };
    let shader = as_host(lowered(&native).expect("lower"));
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 device; skipped");
        return None;
    };
    if wave64 && host_lanes(&host) < 64 {
        let Err(crate::ShaderError::Unsupported { what, .. }) = lowered_on(&native, true) else {
            panic!("translated a wave64 program for a 32-lane host");
        };
        assert_eq!(what, "a wave64 program reading other lanes where every lane is, on a 32-lane host");
        // As two 32-lane halves: console lane numbers and thread ids, and
        // each half sees its own 32 lanes active.
        let halves = as_host(lowered_halves(&native, true, true).expect("translated as halves"));
        let (root, pso) = host.compute_pipeline(&halves).expect("halves pipeline");
        let ud = vec![0u32; halves.user_data_dwords().max(1) as usize];
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(64 * 16)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .expect("dispatch");
        let got: Vec<[u32; 3]> = out[0].chunks(4).take(64).map(|c| [c[0], c[1], c[2]]).collect();
        let want: Vec<[u32; 3]> = (0..64).map(|t| [t, 32, t]).collect();
        assert_eq!(got, want, "wave64 as halves");
        return None;
    }
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(p) => p,
        Err(e) => panic!("the host refused the pipeline (wave64 {wave64}): {e}"),
    };
    let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(64 * 16)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .expect("dispatch");
    let got: Vec<[u32; 3]> = out[0].chunks(4).take(64).map(|c| [c[0], c[1], c[2]]).collect();
    eprintln!("wave64 {wave64}: per thread [lane, active lanes, thread id], 28-35: {:?}, 60-63: {:?}", &got[28..36], &got[60..64]);
    Some(got)
}

/// A lowered compute program as the host runs it.
fn as_host(l: lower::Lowered) -> HostShader {
    HostShader {
        stage: HwStage::Cs,
        dxil: l.dxil,
        user_data: l.user_data,
        static_samplers: Vec::new(),
        samplers: l.samplers,
        root_cbvs: l.root_cbvs,
        root_srvs: l.root_srvs,
        root_uavs: l.root_uavs,
        memory: l.memory,
        memory_writes: l.memory_writes,
        view_addresses: l.view_addresses,
        heap: l.heap,
        heap_writes: l.heap_writes,
        view_marks: l.view_marks,
    }
}

/// Runs a 64-thread wave64 compute program that shares LDS as one wave,
/// translated for a 32-lane host, after checking its words decode to
/// `names`: its UAV (entry 0 of the table at s0), 64 dwords, from 8 runs
/// (a race between the host waves shows only sometimes). None without a
/// device.
fn run_lds64(code: &[u32], names: &[&str]) -> Option<Vec<Vec<u32>>> {
    let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    let decoded: Vec<&str> = crate::gfx10::decode(&bytes)
        .expect("decode")
        .iter()
        .map(|i| i.name().unwrap_or("?"))
        .collect();
    assert_eq!(decoded, names, "the hand-encoded words");
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 8,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }],
        io: StageIo::Cs(ComputeIo {
            // One user SGPR, 512 bytes of LDS.
            rsrc2: 1 << 1 | 1 << 15,
            threads: [64, 1, 1],
        }),
        code: bytes,
    };
    let shader = as_host(lowered_on(&native, true).expect("translated for a 32-lane host"));
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 device; skipped");
        return None;
    };
    let (root, pso) = host.compute_pipeline(&shader).expect("pipeline");
    let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let runs = (0..8)
        .map(|_| {
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(ud.clone()))],
                        views: &[View::RawUav(64 * 4)],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .expect("dispatch");
            out[0][..64].to_vec()
        })
        .collect();
    Some(runs)
}

/// The start of both LDS programs: the UAV at s[40:43], v1 = thread × 4,
/// and each thread's id written to LDS there.
const LDS64_START: [u32; 7] = [
    0xBE84_0300, // s_mov_b32 s4, s0
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0A02,
    0xFA00_0000, // s_load_dwordx4 s[40:43], s[4:5], 0x0
    0x3402_0082, // v_lshlrev_b32 v1, 2, v0
    0xD834_0000,
    0x0000_0001, // ds_write_b32 v1, v0
];
const LDS64_START_NAMES: [&str; 5] =
    ["s_mov_b32", "s_mov_b32", "s_load_dwordx4", "v_lshlrev_b32", "ds_write_b32"];

/// A wave64 group of 64 threads that shares LDS as one wave, as the
/// console compiler leaves it: each thread writes its id to LDS, and with
/// no barrier (one console wave runs in step) reads thread 63 − id's. On a
/// 32-lane host that is two waves, each reading the other's writes: the
/// translation keeps them in step with a barrier after each LDS access, so
/// every thread reads 63 − id. Without the barriers the first wave read
/// zeros (25 November 2026, D3DMetal 4.0 beta 3).
#[test]
fn a_wave64_group_shares_lds_on_32_lanes() {
    let mut code = LDS64_START.to_vec();
    code.extend([
        0x3A04_00BF, // v_xor_b32 v2, 63, v0
        0x3404_0482, // v_lshlrev_b32 v2, 2, v2
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xD8D8_0000,
        0x0300_0002, // ds_read_b32 v3, v2
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xE070_1000,
        0x800A_0301, // buffer_store_dword v3, v1, s[40:43], 0 offen
        0xBF81_0000, // s_endpgm
    ]);
    let mut names = LDS64_START_NAMES.to_vec();
    names.extend(["v_xor_b32", "v_lshlrev_b32", "s_waitcnt", "ds_read_b32", "s_waitcnt", "buffer_store_dword", "s_endpgm"]);
    let Some(runs) = run_lds64(&code, &names) else { return };
    let want: Vec<u32> = (0..64).map(|t| 63 - t).collect();
    for (run, got) in runs.iter().enumerate() {
        assert_eq!(got, &want, "run {run}");
    }
}

/// As [`a_wave64_group_shares_lds_on_32_lanes`], with LDS written in a
/// per-lane branch: threads below 32 copy the slot 32 above theirs into
/// theirs (`if (tid < 32) lds[tid] = lds[tid + 32]`), and after the branch
/// every thread reads slot tid ^ 32. The upper wave reads what the lower
/// one wrote in the branch, where no barrier can go (the upper wave never
/// gets there): the barrier is where the branch ends.
#[test]
fn a_wave64_group_shares_lds_written_in_a_branch() {
    let mut code = LDS64_START.to_vec();
    code.extend([
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0x7D88_00A0, // v_cmp_gt_u32_e32 vcc, 32, v0
        0xBE86_246A, // s_and_saveexec_b64 s[6:7], vcc
        0xBF88_0008, // s_cbranch_execz join
        0x380A_00A0, // v_or_b32 v5, 32, v0
        0x340A_0A82, // v_lshlrev_b32 v5, 2, v5
        0xD8D8_0000,
        0x0600_0005, // ds_read_b32 v6, v5
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xD834_0000,
        0x0000_0601, // ds_write_b32 v1, v6
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0x88FE_067E, // join: s_or_b64 exec, exec, s[6:7]
        0x3A04_00A0, // v_xor_b32 v2, 32, v0
        0x3404_0482, // v_lshlrev_b32 v2, 2, v2
        0xD8D8_0000,
        0x0300_0002, // ds_read_b32 v3, v2
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xE070_1000,
        0x800A_0301, // buffer_store_dword v3, v1, s[40:43], 0 offen
        0xBF81_0000, // s_endpgm
    ]);
    let mut names = LDS64_START_NAMES.to_vec();
    names.extend([
        "s_waitcnt",
        "v_cmp_gt_u32",
        "s_and_saveexec_b64",
        "s_cbranch_execz",
        "v_or_b32",
        "v_lshlrev_b32",
        "ds_read_b32",
        "s_waitcnt",
        "ds_write_b32",
        "s_waitcnt",
        "s_or_b64",
        "v_xor_b32",
        "v_lshlrev_b32",
        "ds_read_b32",
        "s_waitcnt",
        "buffer_store_dword",
        "s_endpgm",
    ]);
    let Some(runs) = run_lds64(&code, &names) else { return };
    // Below 32: slot tid + 32, never written in the branch. From 32: slot
    // tid − 32, which the branch set to (tid − 32) + 32.
    let want: Vec<u32> = (0..64).map(|t| if t < 32 { t + 32 } else { t }).collect();
    for (run, got) in runs.iter().enumerate() {
        assert_eq!(got, &want, "run {run}");
    }
}

/// A wave64 group of 64 threads whose lanes from `keep` on leave first
/// (`if (tid >= keep) return;`, a per-lane branch to the end), and whose
/// other lanes share LDS as one wave: each writes its id, then reads slot
/// keep − 1 − tid. The whole rest of the program is that branch, so the
/// only barrier after the region would come after every access; the
/// translation puts one after each access inside it too, which the lanes
/// that left never reach. Lanes that left store nothing.
fn early_exit_lds(keep: u32) -> Option<Vec<Vec<u32>>> {
    let code = [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0A02,
        0xFA00_0000, // s_load_dwordx4 s[40:43], s[4:5], 0x0
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0x7D88_0080 | (128 + keep), // v_cmp_gt_u32_e32 vcc, keep, v0
        0xBE86_246A, // s_and_saveexec_b64 s[6:7], vcc
        0xBF88_000A, // s_cbranch_execz end
        0xD834_0000,
        0x0000_0001, // ds_write_b32 v1, v0
        0x4C04_0080 | (128 + keep - 1), // v_sub_nc_u32 v2, keep - 1, v0
        0x3404_0482, // v_lshlrev_b32 v2, 2, v2
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xD8D8_0000,
        0x0300_0002, // ds_read_b32 v3, v2
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xE070_1000,
        0x800A_0301, // buffer_store_dword v3, v1, s[40:43], 0 offen
        0xBF81_0000, // end: s_endpgm
    ];
    let names = [
        "s_mov_b32",
        "s_mov_b32",
        "s_load_dwordx4",
        "v_lshlrev_b32",
        "v_cmp_gt_u32",
        "s_and_saveexec_b64",
        "s_cbranch_execz",
        "ds_write_b32",
        "v_sub_nc_u32",
        "v_lshlrev_b32",
        "s_waitcnt",
        "ds_read_b32",
        "s_waitcnt",
        "buffer_store_dword",
        "s_endpgm",
    ];
    run_lds64(&code, &names)
}

/// [`early_exit_lds`] with lanes 48-63 leaving: the second host wave keeps
/// 16 lanes, and lanes 0-15 read what they wrote.
#[test]
fn a_wave64_group_shares_lds_after_lanes_leave() {
    let Some(runs) = early_exit_lds(48) else { return };
    let want: Vec<u32> = (0..64).map(|t| if t < 48 { 47 - t } else { 0 }).collect();
    for (run, got) in runs.iter().enumerate() {
        assert_eq!(got, &want, "run {run}");
    }
}

/// [`early_exit_lds`] with lanes 32-63 leaving: the whole second host wave
/// leaves, and the first wave's barriers must not wait for it.
#[test]
fn a_wave64_group_shares_lds_after_a_whole_host_wave_leaves() {
    let Some(runs) = early_exit_lds(32) else { return };
    let want: Vec<u32> = (0..64).map(|t| if t < 32 { 31 - t } else { 0 }).collect();
    for (run, got) in runs.iter().enumerate() {
        assert_eq!(got, &want, "run {run}");
    }
}

/// A wave32 program of 32 threads translated as for the host (32 lanes),
/// after checking its words decode to `names`: its UAV (entry 0 of the
/// table at s0), 32 dwords. None without a device.
fn run_cs32(code: &[u32], names: &[&str]) -> Option<Vec<u32>> {
    let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    let decoded: Vec<&str> = crate::gfx10::decode(&bytes)
        .expect("decode")
        .iter()
        .map(|i| i.name().unwrap_or("?"))
        .collect();
    assert_eq!(decoded, names, "the hand-encoded words");
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 1,
            threads: [32, 1, 1],
        }),
        code: bytes,
    };
    let shader = as_host(lowered_on(&native, true).expect("translated"));
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 device; skipped");
        return None;
    };
    let (root, pso) = host.compute_pipeline(&shader).expect("pipeline");
    let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(32 * 4)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .expect("dispatch");
    Some(out[0][..32].to_vec())
}

/// A register the compiler reuses for a lane mask and for data, joined at
/// a loop head and read there as data (each lane tests its own bit), as in
/// the title's shadow-ray programs: s10 enters as the mask of lanes below 8
/// (`vcc` of a compare) and comes around the loop as 1 << 4, a number. Each
/// pass adds 1 in the lanes whose bit is set: lanes 0-7 once, lane 4 twice.
#[test]
fn a_register_joining_a_lane_mask_and_data_reads_as_its_number() {
    let code = [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0A02,
        0xFA00_0000, // s_load_dwordx4 s[40:43], s[4:5], 0x0
        0x7D88_0088, // v_cmp_gt_u32_e32 vcc, 8, v0
        0xBE8A_036A, // s_mov_b32 s10, vcc_lo
        0xBE8B_0380, // s_mov_b32 s11, 0
        0x7E08_0280, // v_mov_b32 v4, 0
        0x7E0A_0281, // v_mov_b32 v5, 1
        0x3404_0B00, // loop: v_lshlrev_b32 v2, v0, v5
        0x3604_040A, // v_and_b32 v2, s10, v2
        0x7D8A_0480, // v_cmp_ne_u32_e32 vcc, 0, v2
        0x0206_0A80, // v_cndmask_b32 v3, 0, v5, vcc
        0x4A08_0903, // v_add_nc_u32 v4, v3, v4
        0x800B_810B, // s_add_u32 s11, s11, 1
        0x8F0A_840B, // s_lshl_b32 s10, s11, 4
        0xBF0A_820B, // s_cmp_lt_u32 s11, 2
        0xBF85_FFF7, // s_cbranch_scc1 loop
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xE070_1000,
        0x800A_0401, // buffer_store_dword v4, v1, s[40:43], 0 offen
        0xBF81_0000, // s_endpgm
    ];
    let names = [
        "s_mov_b32",
        "s_mov_b32",
        "s_load_dwordx4",
        "v_cmp_gt_u32",
        "s_mov_b32",
        "s_mov_b32",
        "v_mov_b32",
        "v_mov_b32",
        "v_lshlrev_b32",
        "v_and_b32",
        "v_cmp_ne_u32",
        "v_cndmask_b32",
        "v_add_nc_u32",
        "s_add_u32",
        "s_lshl_b32",
        "s_cmp_lt_u32",
        "s_cbranch_scc1",
        "v_lshlrev_b32",
        "s_waitcnt",
        "buffer_store_dword",
        "s_endpgm",
    ];
    let Some(got) = run_cs32(&code, &names) else { return };
    let want: Vec<u32> = (0..32).map(|t| u32::from(t < 8) + u32::from(t == 4)).collect();
    assert_eq!(got, want);
}
