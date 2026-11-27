//! A plain upscale in place of the last pass of the title's FSR 4
//! (`VICTORY_SIMPLE_UPSCALE=1`; off by default).
//!
//! FSR 4 is a network: eleven passes of int8 dot products on 64-lane waves,
//! then a last one (`compute fdce2a0a9fb1521a`) that applies the network's
//! last layer to the features the eleven wrote and resolves the picture from
//! them and from its history. On an RTX 4070 Ti the eleven take some 130 ms
//! a frame, so they are skipped (`VICTORY_SKIP_PIPELINES`); the last pass
//! then reads no features, gives every pixel the same blend, and what moves
//! leaves trails and looks see-through.
//!
//! In its place this samples the frame as rendered (the pass's first image,
//! `t3`, the 3x3 neighbourhoods it reads) into each image it writes (`u1`,
//! `u6`, `u2`: the picture, its history and a third of the same size), each at
//! its own size, with a light sharpening. No history, no network: no trails,
//! and the edges of the frame as rendered.

use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::s;

use crate::host::{HostList, host};

/// The title's last pass of FSR 4.
pub(crate) const LAST_PASS: &str = "fdce2a0a9fb1521a";

/// Where that pass's images are: root parameter and place in its table, in
/// dwords (the frame it reads, then the three it writes).
pub(crate) const SOURCE_AT: (u8, u32) = (1, 24);
/// The 1920x1080 picture the pass reads besides the frame: the last frame's
/// output (its history), `t9`.
pub(crate) const HISTORY_AT: (u8, u32) = (1, 72);

/// How much of the history each pixel keeps, 0 (none: the plain upscale)
/// to 0.95 (`VICTORY_SIMPLE_TAA`, 0.8 when set without a number; unset:
/// none). The history is first held to the colours around the pixel in the
/// new frame (a box in YCoCg, 3x3 of its texels): what the title dithers
/// from frame to frame (hair, foliage, the edges its jitter moves) settles
/// into its average, and what moved is let go rather than left as a trail.
pub(crate) fn taa() -> f32 {
    static SET: OnceLock<f32> = OnceLock::new();
    *SET.get_or_init(|| match std::env::var("VICTORY_SIMPLE_TAA") {
        Ok(v) => v.trim().parse::<f32>().ok().filter(|v| v.is_finite()).map_or(0.8, |v| v.clamp(0.0, 0.95)),
        Err(_) => 0.0,
    })
}
pub(crate) const TARGETS_AT: [(u8, u32); 3] = [(0, 8), (0, 48), (0, 16)];

/// Whether the pass is replaced.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SIMPLE_UPSCALE").is_some_and(|v| v == "1"))
}

/// How much the result is sharpened, 0 to 1 (`VICTORY_SIMPLE_SHARPEN`, 0.3).
fn sharpen() -> f32 {
    static SET: OnceLock<f32> = OnceLock::new();
    *SET.get_or_init(|| {
        std::env::var("VICTORY_SIMPLE_SHARPEN")
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|v| v.is_finite())
            .map_or(0.3, |v| v.clamp(0.0, 1.0))
    })
}

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), DescriptorTable(SRV(t0)), DescriptorTable(UAV(u0)), DescriptorTable(SRV(t1)), "     "StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR, addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP)"
cbuffer Set : register(b0) { float amount; float keep; };
Texture2D<float4> frame : register(t0);
Texture2D<float4> history : register(t1);
RWTexture2D<float4> target : register(u0);
SamplerState linear_clamp : register(s0);
float3 ycocg(float3 c) { return float3(dot(c, float3(0.25, 0.5, 0.25)), dot(c, float3(0.5, 0, -0.5)), dot(c, float3(-0.25, 0.5, -0.25))); }
float3 rgb(float3 y) { return float3(y.x + y.y - y.z, y.x + y.z, y.x - y.y - y.z); }
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 at : SV_DispatchThreadID) {
    uint w, h;
    target.GetDimensions(w, h);
    if (at.x >= w || at.y >= h) return;
    uint fw, fh;
    frame.GetDimensions(fw, fh);
    float2 uv = (float2(at.xy) + 0.5) / float2(w, h);
    float2 texel = 1.0 / float2(fw, fh);
    float4 c = frame.SampleLevel(linear_clamp, uv, 0);
    float3 n = frame.SampleLevel(linear_clamp, uv + float2(0, -texel.y), 0).rgb;
    float3 s = frame.SampleLevel(linear_clamp, uv + float2(0, texel.y), 0).rgb;
    float3 e = frame.SampleLevel(linear_clamp, uv + float2(texel.x, 0), 0).rgb;
    float3 w4 = frame.SampleLevel(linear_clamp, uv + float2(-texel.x, 0), 0).rgb;
    float3 lo = min(c.rgb, min(min(n, s), min(e, w4)));
    float3 hi = max(c.rgb, max(max(n, s), max(e, w4)));
    float3 sharp = clamp(c.rgb + amount * (4.0 * c.rgb - n - s - e - w4) * 0.25, lo, hi);
    if (keep > 0) {
        // The history held to the new frame's 3x3 neighbourhood (corners
        // too), as a mean and spread in YCoCg.
        float3 m1 = 0, m2 = 0;
        [unroll] for (int y = -1; y <= 1; y++) {
            [unroll] for (int x = -1; x <= 1; x++) {
                float3 v = ycocg(frame.SampleLevel(linear_clamp, uv + float2(x, y) * texel, 0).rgb);
                m1 += v; m2 += v * v;
            }
        }
        m1 /= 9; m2 /= 9;
        float3 sigma = sqrt(max(m2 - m1 * m1, 0));
        uint hw, hh;
        history.GetDimensions(hw, hh);
        float3 old = history.SampleLevel(linear_clamp, uv, 0).rgb;
        float3 held = rgb(clamp(ycocg(old), m1 - 1.25 * sigma, m1 + 1.25 * sigma));
        bool usable = hw == w && hh == h && all(isfinite(old));
        sharp = usable ? lerp(sharp, held, keep) : sharp;
    }
    target[at.xy] = float4(sharp, c.a);
}
"#;

struct Pass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Pass {}
// SAFETY: as above.
unsafe impl Sync for Pass {}

fn pass() -> Option<&'static Pass> {
    static PASS: OnceLock<Option<Pass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let host = host()?;
        let made = (|| -> windows::core::Result<Pass> {
            let code = crate::host::compile(SOURCE, s!("main"), s!("cs_5_1"))?;
            // SAFETY: the root signature is inside the shader; plain creations.
            unsafe {
                let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &code)?;
                let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
                    ..Default::default()
                };
                let pso = host.device.CreateComputePipelineState(&desc)?;
                Ok(Pass { root, pso })
            }
        })();
        match made {
            Ok(p) => {
                tracing::info!("plain upscale in place of FSR 4's last pass (VICTORY_SIMPLE_UPSCALE=1)");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(error = %e, "the plain upscale could not be made; FSR 4's last pass runs");
                None
            }
        }
    })
    .as_ref()
}

/// One image the pass writes: its view in the heap the list has set, and its
/// size.
pub(crate) struct Target {
    pub(crate) view: D3D12_GPU_DESCRIPTOR_HANDLE,
    pub(crate) size: [u32; 2],
}

/// Records the upscale from `frame` (a view in the heap the list has set)
/// into each of `targets`, in place of the title's pass. Replaces the list's
/// compute root signature and pipeline; false if it recorded nothing (the
/// title's pass then runs).
pub(crate) fn instead(list: &HostList, frame: D3D12_GPU_DESCRIPTOR_HANDLE, history: Option<D3D12_GPU_DESCRIPTOR_HANDLE>, targets: &[Target]) -> bool {
    let Some(pass) = pass() else {
        return false;
    };
    if targets.is_empty() {
        return false;
    }
    let keep = if history.is_some() { taa() } else { 0.0 };
    let set = [sharpen().to_bits(), keep.to_bits()];
    let l = &list.list;
    // SAFETY: an open list with the heap of the views set; the title's pass
    // was bound, so its images are in the states its views need.
    unsafe {
        l.SetComputeRootSignature(&pass.root);
        l.SetPipelineState(&pass.pso);
        l.SetComputeRoot32BitConstants(0, 2, set.as_ptr().cast(), 0);
        l.SetComputeRootDescriptorTable(1, frame);
        // Without a history the frame stands in (its size differs: unused).
        l.SetComputeRootDescriptorTable(3, history.unwrap_or(frame));
        for t in targets {
            l.SetComputeRootDescriptorTable(2, t.view);
            l.Dispatch(t.size[0].div_ceil(8), t.size[1].div_ceil(8), 1);
        }
    }
    true
}
