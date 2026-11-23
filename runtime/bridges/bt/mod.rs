// SPDX-License-Identifier: MIT
//! bt/: Bluetooth (docs/hardware.md "Bluetooth"). One HCI controller for the guest's transport ([`hci::Hci`]) on a
//! back end: the phone's HAL over /dev/hwbinder (HIDL, [`hci_hidl`]) or /dev/binder (stable AIDL, [`hci_aidl`]), or
//! a stand-in ([`hci_sim`]).
//!
//! ```ignore
//! use onebridges::bt::{self, hci::{Hci, HciState, HciFactory}};
//! let factory: HciFactory = match bt::hci_hidl::probe_quiet() {
//!     h if h > 0 => bt::hci_hidl::open_hidl,
//!     _ if bt::hci_aidl::probe() > 0 => bt::hci_aidl::open_aidl,
//!     _ => bt::hci_sim::open_sim,
//! };
//! let hci = factory(Box::new(|kind, packet| ...), Box::new(|state: HciState| ...)).unwrap();
//! hci.open()?; hci.send(1, &command)?; hci.close();
//! let mac = bt::hci::bt_address("/mnt/vendor/efs/bluetooth/bt_addr");
//! ```

pub mod hci; // bt/hci.c
pub mod hci_aidl; // bt/hci_hidl.c's AIDL part, its own module in Rust
pub mod hci_hidl; // bt/hci_hidl.c (HIDL)
pub mod hci_sim; // bt/hci_sim.c
pub mod hidl_parcel; // bt/hidl_parcel.c (the generic HIDL wire format: hal::hidl)
pub mod le_scan; // bt/le_scan.c
