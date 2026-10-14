//! What every decode rung answers to [`Decoder`](crate::video::Decoder), and the six
//! decoders that answer it.
//!
//! A hardware rung reports stream damage as `Ok(None)` plus
//! [`Rung::take_recovery_request`]; its `Err` is a decoder fault and counts toward
//! demotion. The CPU rung and PyroWave have nothing below them, so their `Err`
//! goes to the pump. Only the Vulkan rung has a timeline; the defaults answer for
//! the others.

use anyhow::Result;

use crate::video::DecodedImage;
use crate::video_caps::NativeRung;
use crate::video_software::SoftwareDecoder;
use crate::video_types::DecodeHealth;
use crate::video_vk_native::NativeVulkanDecoder;

pub(crate) trait Rung {
    /// The ladder rung this is. `None` for PyroWave: no ladder holds it.
    fn id(&self) -> Option<NativeRung>;

    /// The decoder as the init log names it.
    fn name(&self) -> &'static str;

    /// Feed one access unit. `user_flags` and `complete` are wire facts only PyroWave
    /// reads; every other rung takes whole units.
    fn decode(
        &mut self,
        au: &[u8],
        user_flags: u32,
        complete: bool,
    ) -> Result<Option<DecodedImage>>;

    /// Drain the re-anchor ask the last `decode` raised by concealing damage.
    fn take_recovery_request(&mut self) -> bool {
        false
    }

    /// Decode-integrity counters. `None` is "cannot see corruption"; `Some(default)`
    /// is "looked and saw none".
    fn health(&self) -> Option<DecodeHealth> {
        None
    }

    /// Drop the planner's damaged-chain marks. A rung with no planner has none.
    fn forgive_unclean(&mut self) {}

    /// Wait for a decode on the rung's timeline. `false` declines the sample.
    fn wait_timeline(&mut self, _sem: u64, _value: u64, _timeout_ns: u64) -> bool {
        false
    }

    /// Whether a timeline decode is complete now. `true` where nothing is pending.
    fn timeline_done(&mut self, _sem: u64, _value: u64) -> bool {
        true
    }

    /// The timeline wait is also the media clock boost (Intel on i915).
    fn boosted(&self) -> bool {
        false
    }

    /// Newest planned decode-order ordinal. 0 on a rung that stamps none.
    fn decode_order(&self) -> u64 {
        0
    }
}

impl Rung for NativeVulkanDecoder {
    fn id(&self) -> Option<NativeRung> {
        Some(NativeRung::Vulkan)
    }

    fn name(&self) -> &'static str {
        NativeRung::Vulkan.name()
    }

    fn decode(&mut self, au: &[u8], _: u32, _: bool) -> Result<Option<DecodedImage>> {
        Ok(Self::decode(self, au)?.map(DecodedImage::NativeVk))
    }

    fn take_recovery_request(&mut self) -> bool {
        Self::take_recovery_request(self)
    }

    fn health(&self) -> Option<DecodeHealth> {
        Some(Self::health(self))
    }

    fn forgive_unclean(&mut self) {
        Self::forgive_unclean(self);
    }

    fn wait_timeline(&mut self, sem: u64, value: u64, timeout_ns: u64) -> bool {
        Self::wait_timeline(self, sem, value, timeout_ns)
    }

    fn timeline_done(&mut self, sem: u64, value: u64) -> bool {
        Self::timeline_done(self, sem, value)
    }

    fn boosted(&self) -> bool {
        Self::boosted(self)
    }

    fn decode_order(&self) -> u64 {
        Self::decode_order(self)
    }
}

// libva has no per-picture decode-status query: `health` reports concealment and
// refusals from the planner, and `failed` stays 0.
#[cfg(target_os = "linux")]
impl Rung for crate::video_vaapi_native::NativeVaapiDecoder {
    fn id(&self) -> Option<NativeRung> {
        Some(NativeRung::Vaapi)
    }

    fn name(&self) -> &'static str {
        Self::name(self)
    }

    fn decode(&mut self, au: &[u8], _: u32, _: bool) -> Result<Option<DecodedImage>> {
        Ok(Self::decode(self, au)?.map(DecodedImage::NativeDmabuf))
    }

    fn take_recovery_request(&mut self) -> bool {
        Self::take_recovery_request(self)
    }

    fn health(&self) -> Option<DecodeHealth> {
        Some(Self::health(self))
    }

    fn forgive_unclean(&mut self) {
        Self::forgive_unclean(self);
    }
}

#[cfg(target_os = "linux")]
impl Rung for crate::video_v4l2::NativeV4l2Decoder {
    fn id(&self) -> Option<NativeRung> {
        Some(NativeRung::V4l2)
    }

    fn name(&self) -> &'static str {
        Self::name(self)
    }

    fn decode(&mut self, au: &[u8], _: u32, _: bool) -> Result<Option<DecodedImage>> {
        Self::decode(self, au)
    }

    fn take_recovery_request(&mut self) -> bool {
        Self::take_recovery_request(self)
    }

    fn health(&self) -> Option<DecodeHealth> {
        Some(Self::health(self))
    }

    fn forgive_unclean(&mut self) {
        Self::forgive_unclean(self);
    }
}

// DXVA has no per-picture status read either: `health` is the planner's view.
#[cfg(windows)]
impl Rung for crate::video_d3d11_native::NativeD3d11Decoder {
    fn id(&self) -> Option<NativeRung> {
        Some(NativeRung::D3d11va)
    }

    fn name(&self) -> &'static str {
        Self::name(self)
    }

    fn decode(&mut self, au: &[u8], _: u32, _: bool) -> Result<Option<DecodedImage>> {
        Ok(Self::decode(self, au)?.map(DecodedImage::D3d11))
    }

    fn take_recovery_request(&mut self) -> bool {
        Self::take_recovery_request(self)
    }

    fn health(&self) -> Option<DecodeHealth> {
        Some(Self::health(self))
    }

    fn forgive_unclean(&mut self) {
        Self::forgive_unclean(self);
    }
}

// Nothing else decodes PyroWave: a failure renegotiates the codec.
#[cfg(feature = "pyrowave")]
impl Rung for crate::video_pyrowave::PyroWaveDecoder {
    fn id(&self) -> Option<NativeRung> {
        None
    }

    fn name(&self) -> &'static str {
        "pyrowave"
    }

    fn decode(
        &mut self,
        au: &[u8],
        user_flags: u32,
        complete: bool,
    ) -> Result<Option<DecodedImage>> {
        let aligned = user_flags & punktfunk_core::packet::USER_FLAG_CHUNK_ALIGNED != 0;
        Ok(self
            .decode_frame(au, aligned, complete)?
            .map(DecodedImage::PyroWave))
    }
}

// Last in every ladder, and the only rung that can fail to exist for a codec:
// `last_rung_verdict` answers that.
impl Rung for SoftwareDecoder {
    fn id(&self) -> Option<NativeRung> {
        Some(NativeRung::Software)
    }

    fn name(&self) -> &'static str {
        NativeRung::Software.name()
    }

    fn decode(&mut self, au: &[u8], _: u32, _: bool) -> Result<Option<DecodedImage>> {
        Ok(Self::decode(self, au)?.map(DecodedImage::Cpu))
    }
}
