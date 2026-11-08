# screen_fix video pipeline static review

> Historical design review. For the current implementation and acceptance status, see [SCREEN_FIX_ARCHITECTURE.md](SCREEN_FIX_ARCHITECTURE.md).

Basis: SCREEN_FIX_ARCHITECTURE.md v1, SCREEN_FIX_SPEC.md v1, and Microsoft's API contracts. At the time of this review the project folder contained documentation without video pipeline source code. This review covers the design. No build or runtime check was performed.

**Conclusion.** WGC → D3D11 → hardware H.264 MFT → MP4 is compatible at the API level under the conditions below. The architecture can serve as the basis for a vertical slice. Hardware support, absence of CPU readback, index reproducibility, and performance remain unverified.

## Implementation conditions

1. **Display and Start.** Programmatic display selection through CreateForMonitor requires Windows 10 1903, build 18362, or later. Before Start, verify WGC, Media Foundation, the adapter, and the profile. Check time to first frame on a static desktop. A session without a usable frame must not publish successful output.
2. **WGC leases and owned textures.** The frame belongs to the WGC pool. Record valid content bounds and hold the lease until GPU reading is safe, or establish equivalent command ordering. The encoder receives an owned NV12 surface. Return it only after MFT releases it. Successful ProcessInput or output from ProcessOutput does not establish that the specific surface is free. Verify BGRA/NV12 view support and SDR color conversion.
3. **Hardware MFT.** Enumerate hardware encoders for the selected adapter LUID through MFTEnum2. Verify D3D11 awareness, pass the DXGI device manager, and negotiate actual NV12 and H.264 types. Enumeration and texture creation do not guarantee sample acceptance. On dual-GPU laptops, verify adapter choice and the entire path without CPU readback. Reject unsupported hardware profiles without silently switching to CPU encoding.
4. **MFT and Stop.** Drive an async MFT through NeedInput and HaveOutput. At Stop, finish supplying accepted frames, signal end of input, and collect output through DrainComplete. Finalize the MP4 sink before publishing the manifest. Flush discards buffered samples and is unsuitable for normal Stop.
5. **H.264 and MP4.** Negotiate compressed media type and SPS/PPS with the sink. Begin with a decodable access point. The sink can skip initial samples if configuration is missing. Consider disabling B-frames for the first profile if the MFT guarantees this. Otherwise preserve DTS separately from PTS and retain decode order.
6. **Finalized container index.** Distinguish WGC observation time, encoder input PTS, and finalized MP4 presentation time. Input acceptance alone is insufficient for publishing the final index. After close, reconcile samples, start offset, timescale rounding, and durations. Every sample must map unambiguously to its observation. Verify decoded first frame, static interval, transitions, and the last partial interval.

## Vertical-slice acceptance

On Windows 10 and Windows 11, record a static display followed by distinct visible changes, then stop normally. Test Stop before the first frame separately. Confirm the selected hardware MFT, no full-frame CPU readback during capture, bounded pools, MP4 decodability from start to finish, and one-to-one correspondence for indexed samples. Measure load separately while the target application is running.

Independent review confirmed gaps in surface lifetime, drain, and the final index. These were design and verification requirements at the time of review because source code was not available.
