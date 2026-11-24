// SPDX-License-Identifier: MIT
//! The host video encoder bridge (README.md here; docs/hardware.md, "Video encoder"): xnuhost's
//! "droidvm,virtio-media" device ([`media_device`]) backed by a real encoder. iOS's VideoToolbox paravirtualized
//! encoder (the guest kernel archive codec/, UniXNU-droidvm's DroidVMVTParavirt.cpp + DroidVMVTHost.cpp +
//! DroidVMVirtioMedia.c) drives a stateful V4L2 encoder; this bridge is the host end of that V4L2 codec. It feeds the
//! NV12 frames VideoToolbox gives it to the backend ([`backend`]) and returns the HEVC/H.264 bitstream (Annex-B NAL
//! units), so CMPhoto writes HEIC.
//!
//! xnuhost (static) links the device, the helper backend and the stand-in; MediaCodec ([`backend_ndk`], Android only)
//! is in the two dynamic programs, oneios-encoder ([`helper`]) and encctl ([`encctl`]); build.rs links libmediandk
//! into them only.

pub use onecamera::pod;

pub mod backend;
pub mod backend_helper;
pub mod backend_ndk;
pub mod backend_stub;
pub mod decode_ndk;
pub mod decoder;
pub mod encctl;
pub mod encctl_decode;
pub mod helper;
pub mod link;
pub mod media_device;
pub mod proto;
pub mod switches;
