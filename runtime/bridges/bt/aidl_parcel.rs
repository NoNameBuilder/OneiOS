// SPDX-License-Identifier: MIT
//! The Bluetooth HAL's stable-AIDL parcels (android.hardware.bluetooth.IBluetoothHci, for phones with no HIDL HAL:
//! the Galaxy A56): the client's requests and what its callback object answers. The wire format is
//! [`crate::hal::aidl`] (libbinder's Parcel: the interface token, byte[] as an int32 length and the bytes padded to
//! 4, a binder object followed by its stability word). Ported from the A56 fork's bt/hidl_parcel.c aidl_* part
//! (Musicalista/OneiOS-A56 26a2276).
//!
//! The transaction codes are FIRST_CALL_TRANSACTION (1) + the declaration order of AOSP's
//! hardware/interfaces/bluetooth/aidl/android/hardware/bluetooth/IBluetoothHci.aidl and IBluetoothHciCallbacks.aidl,
//! as frozen in aidl_api/android.hardware.bluetooth/1 (the only frozen version; checked against AOSP main
//! 2026-11-24). A stable interface only appends methods, so they hold for later versions too.

use super::hidl_parcel::Callback;
use crate::hal::aidl::{self, Parcel, Reader};

pub const HCI: &str = "android.hardware.bluetooth.IBluetoothHci";
pub const HCI_INSTANCE: &str = "android.hardware.bluetooth.IBluetoothHci/default";
pub const CALLBACKS: &str = "android.hardware.bluetooth.IBluetoothHciCallbacks";
/// The interface version our callback object implements, and its hash (aidl_api/android.hardware.bluetooth/1/.hash).
pub const VERSION: i32 = 1;
pub const HASH: &str = "f0eb2eb6d3b970cef8fb1b6e22c4ec39a4da5119";

// IBluetoothHci, in IBluetoothHci.aidl's order.
pub const HCI_CLOSE: u32 = 1; // close()
pub const HCI_INITIALIZE: u32 = 2; // initialize(in IBluetoothHciCallbacks callback)
pub const HCI_SEND_ACL: u32 = 3; // sendAclData(in byte[] data)
pub const HCI_SEND_COMMAND: u32 = 4; // sendHciCommand(in byte[] command)
pub const HCI_SEND_ISO: u32 = 5; // sendIsoData(in byte[] data)
pub const HCI_SEND_SCO: u32 = 6; // sendScoData(in byte[] data)
                                 // IBluetoothHciCallbacks, in IBluetoothHciCallbacks.aidl's order.
pub const CB_ACL: u32 = 1; // aclDataReceived(in byte[] data)
pub const CB_EVENT: u32 = 2; // hciEventReceived(in byte[] event)
pub const CB_INIT_COMPLETE: u32 = 3; // initializationComplete(in Status status): Status is an int enum, SUCCESS 0
pub const CB_ISO: u32 = 4; // isoDataReceived(in byte[] data)
pub const CB_SCO: u32 = 5; // scoDataReceived(in byte[] data)

/// The largest parcel a send makes: the token and an HCI packet of up to 64 KiB (the queue's bound, 0xffff).
pub const PARCEL_MAX: usize = 0x1_0000 + 256;

/// IServiceManager.getService(name) for the HAL (servicemanager is a system process: the 'SYST' token).
pub fn get_service(p: &mut Parcel) {
    p.clear();
    p.put_token(crate::hal::SERVICE_MANAGER);
    p.put_str(HCI_INSTANCE);
}

/// initialize(callbacks): our callback object, VINTF-stable (the HAL is a VINTF service and calls only binders at
/// least that stable). `header`: the token's libbinder marker ([`aidl::HEADER_SYST`] or [`aidl::HEADER_VNDR`]).
pub fn initialize(p: &mut Parcel, header: u32, ours: u64) {
    p.clear();
    p.put_token_header(HCI, header);
    p.put_local(ours);
}

/// The IBluetoothHci call that carries an H4 packet of type `kind` (1 command, 2 ACL, 3 SCO, 5 ISO).
pub fn send_code(kind: u8) -> Option<u32> {
    match kind {
        1 => Some(HCI_SEND_COMMAND),
        2 => Some(HCI_SEND_ACL),
        3 => Some(HCI_SEND_SCO),
        5 => Some(HCI_SEND_ISO),
        _ => None,
    }
}

/// sendHciCommand, sendAclData, sendScoData, sendIsoData: the packet as a byte[].
pub fn send(p: &mut Parcel, header: u32, packet: &[u8]) {
    p.clear();
    p.put_token_header(HCI, header);
    p.put_bytes(packet);
}

/// close().
pub fn close(p: &mut Parcel, header: u32) {
    p.clear();
    p.put_token_header(HCI, header);
}

/// What the callback object answers to transaction `code` with request `r`: Ok with the reply made in `reply`, or
/// the status to reply with (BAD_TYPE: not our interface's token or a malformed parcel, as libbinder_ndk answers;
/// UNKNOWN_TRANSACTION: a method we do not have).
pub fn callback_transact<'a>(code: u32, r: &mut Reader<'a>, reply: &mut Parcel) -> Result<Callback<'a>, i32> {
    reply.clear();
    if let Some(standard) = aidl::standard(code, CALLBACKS, VERSION, HASH) {
        *reply = standard; // the descriptor (the HAL's libbinder_ndk asks for it to check the class), ping, version
        return Ok(Callback::Base);
    }
    if !r.token(CALLBACKS) {
        return Err(aidl::BAD_TYPE);
    }
    let callback = match code {
        CB_INIT_COMPLETE => {
            let status = r.i32();
            if r.is_bad() {
                return Err(aidl::BAD_TYPE);
            }
            Callback::InitComplete(status)
        }
        CB_EVENT | CB_ACL | CB_SCO | CB_ISO => {
            let packet = r.bytes().ok_or(aidl::BAD_TYPE)?;
            let h4 = match code {
                CB_EVENT => 4,
                CB_ACL => 2,
                CB_SCO => 3,
                _ => 5,
            };
            Callback::Packet(h4, packet)
        }
        _ => return Err(aidl::UNKNOWN_TRANSACTION),
    };
    reply.put_ok();
    Ok(callback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hal::aidl::{FlatObject, Object, HEADER_SYST, HEADER_VNDR, LOCAL_FLAGS, TYPE_BINDER};

    /// libbinder's token: strict mode 0x80000000, work source -1, the header, the descriptor in UTF-16.
    fn token_bytes(descriptor: &str, header: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0x8000_0000u32.to_ne_bytes());
        b.extend_from_slice(&(-1i32).to_ne_bytes());
        b.extend_from_slice(&header.to_ne_bytes());
        b.extend_from_slice(&(descriptor.len() as i32).to_ne_bytes());
        for c in descriptor.bytes() {
            b.extend_from_slice(&[c, 0]);
        }
        b.extend_from_slice(&[0, 0]);
        b.resize((b.len() + 3) & !3, 0);
        b
    }

    #[test]
    fn codes_are_aosps_declaration_order() {
        // IBluetoothHci.aidl: close, initialize, sendAclData, sendHciCommand, sendIsoData, sendScoData.
        let hci = [HCI_CLOSE, HCI_INITIALIZE, HCI_SEND_ACL, HCI_SEND_COMMAND, HCI_SEND_ISO, HCI_SEND_SCO];
        assert_eq!(hci, [1, 2, 3, 4, 5, 6]);
        // IBluetoothHciCallbacks.aidl: aclDataReceived, hciEventReceived, initializationComplete, isoDataReceived,
        // scoDataReceived.
        assert_eq!([CB_ACL, CB_EVENT, CB_INIT_COMPLETE, CB_ISO, CB_SCO], [1, 2, 3, 4, 5]);
        assert_eq!(
            [1, 2, 3, 5, 4].map(send_code),
            [Some(HCI_SEND_COMMAND), Some(HCI_SEND_ACL), Some(HCI_SEND_SCO), Some(HCI_SEND_ISO), None]
        );
    }

    #[test]
    fn requests_byte_for_byte() {
        let mut p = Parcel::new();
        get_service(&mut p);
        let mut want = token_bytes("android.os.IServiceManager", HEADER_SYST);
        want.extend_from_slice(&token_bytes(HCI_INSTANCE, 0)[12..]); // a String: length, UTF-16, padding
        assert_eq!(p.bytes(), &want[..]);

        initialize(&mut p, HEADER_SYST, 0x4f6e_6942);
        let token = token_bytes(HCI, HEADER_SYST);
        assert_eq!(&p.bytes()[..token.len()], &token[..]);
        assert_eq!(p.object_offsets(), &[token.len() as u64]);
        let o = FlatObject::parse(&p.bytes()[token.len()..token.len() + 24]);
        assert_eq!(
            o,
            FlatObject { kind: TYPE_BINDER, flags: LOCAL_FLAGS, binder_or_handle: 0x4f6e_6942, cookie: 0x4f6e_6942 }
        );
        assert_eq!(&p.bytes()[token.len() + 24..], &0x3fi32.to_ne_bytes()); // VINTF stability
        let mut r = p.reader();
        assert!(r.token(HCI) && r.binder() == Object::Local(0x4f6e_6942) && !r.is_bad());

        initialize(&mut p, HEADER_VNDR, 1);
        assert_eq!(p.reader().token_header(HCI), Some(HEADER_VNDR));

        let reset = [0x03u8, 0x0c, 0x00];
        send(&mut p, HEADER_SYST, &reset);
        let mut want = token_bytes(HCI, HEADER_SYST);
        want.extend_from_slice(&[3, 0, 0, 0, 0x03, 0x0c, 0x00, 0]);
        assert_eq!(p.bytes(), &want[..]);
        assert!(p.object_offsets().is_empty());

        close(&mut p, HEADER_SYST);
        assert_eq!(p.bytes(), &token_bytes(HCI, HEADER_SYST)[..]);

        // An ACL packet past the default parcel limit fits a send parcel with PARCEL_MAX.
        let mut p = Parcel::new();
        p.set_limit(PARCEL_MAX);
        let acl = vec![7u8; 0xffff];
        send(&mut p, HEADER_SYST, &acl);
        assert!(!p.is_overflow());
        let mut r = p.reader();
        assert!(r.token(HCI) && r.bytes() == Some(&acl[..]));
    }

    #[test]
    fn callbacks() {
        let complete = [0x0eu8, 0x04, 0x01, 0x03, 0x0c, 0x00];
        let mut reply = Parcel::new();
        let packet = |data: &[u8]| {
            let mut p = Parcel::with_token(CALLBACKS);
            p.put_bytes(data);
            p
        };
        for (code, h4) in [(CB_EVENT, 4u8), (CB_ACL, 2), (CB_SCO, 3), (CB_ISO, 5)] {
            let p = packet(&complete);
            assert_eq!(callback_transact(code, &mut p.reader(), &mut reply), Ok(Callback::Packet(h4, &complete[..])));
            assert_eq!(reply.bytes(), &[0, 0, 0, 0]); // Status::ok()
        }
        let mut p = Parcel::with_token(CALLBACKS);
        p.put_i32(3); // HARDWARE_INITIALIZATION_ERROR
        assert_eq!(callback_transact(CB_INIT_COMPLETE, &mut p.reader(), &mut reply), Ok(Callback::InitComplete(3)));
        // Another interface's token, a truncated byte[], a method we do not have.
        let mut p = Parcel::with_token(HCI);
        p.put_bytes(&complete);
        assert_eq!(callback_transact(CB_EVENT, &mut p.reader(), &mut reply), Err(aidl::BAD_TYPE));
        let mut p = Parcel::with_token(CALLBACKS);
        p.put_i32(100);
        p.put_i32(0);
        assert_eq!(callback_transact(CB_ACL, &mut p.reader(), &mut reply), Err(aidl::BAD_TYPE));
        let p = Parcel::with_token(CALLBACKS);
        assert_eq!(callback_transact(CB_INIT_COMPLETE, &mut p.reader(), &mut reply), Err(aidl::BAD_TYPE));
        assert_eq!(callback_transact(6, &mut p.reader(), &mut reply), Err(aidl::UNKNOWN_TRANSACTION));
        // IBinder's own: the descriptor (no status header), ping, the version and hash of frozen version 1.
        let empty = Parcel::new();
        assert_eq!(callback_transact(aidl::INTERFACE, &mut empty.reader(), &mut reply), Ok(Callback::Base));
        assert_eq!(reply.reader().string(95).as_deref(), Some(CALLBACKS));
        assert_eq!(callback_transact(aidl::PING, &mut empty.reader(), &mut reply), Ok(Callback::Base));
        assert_eq!(reply.size(), 0);
        let p = Parcel::with_token(CALLBACKS);
        assert_eq!(callback_transact(aidl::GET_VERSION, &mut p.reader(), &mut reply), Ok(Callback::Base));
        let mut r = reply.reader();
        assert!(r.status().is_ok() && r.i32() == 1);
        assert_eq!(callback_transact(aidl::GET_HASH, &mut p.reader(), &mut reply), Ok(Callback::Base));
        let mut r = reply.reader();
        assert!(r.status().is_ok() && r.string(95).as_deref() == Some(HASH));
    }
}
