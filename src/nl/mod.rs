// socketcan/src/nl/mod.rs
//
// Netlink access to the SocketCAN interfaces.
//
// This file is part of the Rust 'socketcan-rs' library.
//
// Licensed under the MIT license:
//   <LICENSE or http://opensource.org/licenses/MIT>
// This file may not be copied, modified, or distributed except according
// to those terms.

//! CAN Netlink access
//!
//! The netlink module contains the netlink-based management capabilities of
//! the socketcan crate. Netlink is a socket-based mechanism, similar to
//! Unix-domain sockets, which allows a user-space program communicate with
//! the kernel.
//!
//! In the case of the SocketCAN subsystem, it allows an application to query
//! or set the paramaters of a CAN interface, such as the bitrate, the control
//! mode bits, and so forth. It also allows the application to get statistics
//! from the inerface and send commands to the interface, such as performing a
//! bus restart
//!
//! Unfortunately, the SocketCAN netlink API does not appear to be documented
//! _anywhere_. The netlink functional summary on the SocketCAN page is here:
//!
//! <https://www.kernel.org/doc/html/latest/networking/can.html#netlink-interface-to-set-get-devices-properties>
//!
//! The CAN netlink header file for the Linux kernel has the definition of
//! the constants and data structures that are sent back and forth to the
//! kernel over nelink. It can be found in the Linux sources here:
//!
//! <https://github.com/torvalds/linux/blob/master/include/uapi/linux/can/netlink.h?ts=4>
//!
//! The corresponding kernel code that receives and processes messages from
//! userspace is useful to help figure out what the kernel expects. It's here:
//!
//! <https://github.com/torvalds/linux/blob/master/drivers/net/can/dev/netlink.c?ts=4>
//! <https://github.com/torvalds/linux/blob/master/drivers/net/can/dev/dev.c?ts=4>
//!
//! The main Linux user-space client to communicate with network interfaces,
//! including CAN is _iproute2_. The CAN-specific code for it is here:
//!
//! <https://github.com/iproute2/iproute2/blob/main/ip/iplink_can.c?ts=4>
//!
//! There is also a C user-space library for SocketCAN, which primarily
//! deals with the Netlink interface. There are several forks, but one of
//! the later ones with updated documents is here:
//!
//! <https://github.com/lalten/libsocketcan>
//!

use neli::{
    attr::Attribute,
    consts::{
        nl::{NlType, NlmF, NlmFFlags},
        rtnl::{Arphrd, RtAddrFamily, Rtm},
        rtnl::{Iff, IffFlags, Ifla, IflaInfo},
        socket::NlFamily,
    },
    err::NlError,
    nl::{NlPayload, Nlmsghdr},
    rtnl::{Ifinfomsg, Rtattr},
    socket::NlSocketHandle,
    types::{Buffer, RtBuffer},
    FromBytes, ToBytes,
};
use nix::{self, net::if_::if_nametoindex, unistd};
use rt::IflaCan;
use std::{
    ffi::CString,
    fmt::Debug,
    os::raw::{c_int, c_uint},
};

/// A result for Netlink errors.
type NlResult<T> = Result<T, NlError>;

/// Gets a byte slice for any sized variable.
///
/// Note that this should normally be unsafe, but since we're only
/// using it internally for types sent to the kernel, it's OK.
fn as_bytes<T: Sized>(val: &T) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts::<'_, u8>(val as *const _ as *const u8, std::mem::size_of::<T>())
    }
}

/// The details of the interface which can be obtained with the
/// `CanInterface::detail()` function.
#[allow(missing_copy_implementations)]
#[derive(Debug, Default, Clone)]
pub struct InterfaceDetails {
    /// The name of the interface
    pub name: Option<String>,
    /// The index of the interface
    pub index: c_uint,
    /// Whether the interface is currently up
    pub is_up: bool,
    /// The MTU size of the interface (Standard or FD frames support)
    pub mtu: Option<Mtu>,
    /// The CAN bit timing parameters
    pub bit_timing: Option<rt::can_bittiming>,
}

impl InterfaceDetails {
    /// Creates a new set of interface details with the specified `index`.
    pub fn new(index: c_uint) -> Self {
        Self {
            index,
            ..Self::default()
        }
    }
}

/// The MTU size for the interface
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Mtu {
    /// Standard CAN frame, 8-byte data (16-byte total)
    Standard = 16,
    /// FD CAN frame, 64-byte data (64-byte total)
    Fd = 72,
}

impl TryFrom<u32> for Mtu {
    type Error = std::io::Error;

    fn try_from(val: u32) -> Result<Self, Self::Error> {
        match val {
            16 => Ok(Mtu::Standard),
            72 => Ok(Mtu::Fd),
            _ => Err(std::io::Error::from(std::io::ErrorKind::InvalidData)),
        }
    }
}

/// Low-level Netlink CAN struct bindings.
mod rt;

// ===== CanCtrlMode(s) =====

///
/// CAN control modes
///
/// Note that these correspond to the bit _numbers_ for the control mode bits.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CanCtrlMode {
    /// Loopback mode
    Loopback,
    /// Listen-only mode
    ListenOnly,
    /// Triple sampling mode
    TripleSampling,
    /// One-Shot mode
    OneShot,
    /// Bus-error reporting
    BerrReporting,
    /// CAN FD mode
    Fd,
    /// Ignore missing CAN ACKs
    PresumeAck,
    /// CAN FD in non-ISO mode
    NonIso,
    /// Classic CAN DLC option
    CcLen8Dlc,
}

impl CanCtrlMode {
    /// Get the mask for the specific control mode
    pub fn mask(&self) -> u32 {
        1u32 << (*self as u32)
    }
}

/// The collection of control modes
#[derive(Debug, Default, Clone, Copy)]
pub struct CanCtrlModes(rt::can_ctrlmode);

impl CanCtrlModes {
    /// Create a set of CAN control modes from a mask and set of flags.
    pub fn new(mask: u32, flags: u32) -> Self {
        Self(rt::can_ctrlmode { mask, flags })
    }

    /// Create the set of mode flags for a single mode
    pub fn from_mode(mode: CanCtrlMode, on: bool) -> Self {
        let mask = mode.mask();
        let flags = if on { mask } else { 0 };
        Self::new(mask, flags)
    }

    /// Adds a mode flag to the existing set of modes.
    pub fn add(&mut self, mode: CanCtrlMode, on: bool) {
        let mask = mode.mask();
        self.0.mask |= mask;
        if on {
            self.0.flags |= mask;
        }
    }

    /// Clears all of the mode flags in the collection
    pub fn clear(&mut self) {
        self.0 = rt::can_ctrlmode::default();
    }
}

impl From<rt::can_ctrlmode> for CanCtrlModes {
    fn from(mode: rt::can_ctrlmode) -> Self {
        Self(mode)
    }
}

impl From<CanCtrlModes> for rt::can_ctrlmode {
    fn from(mode: CanCtrlModes) -> Self {
        mode.0
    }
}

// ===== CanInterface =====

/// SocketCAN Netlink CanInterface
///
/// Controlled through the kernel's Netlink interface, CAN devices can be
/// brought up or down or configured or queried through this.
///
/// Note while that this API is designed in an RAII-fashion, it cannot really
/// make the same guarantees: It is entirely possible for another user/process
/// to modify, remove and re-add an interface while you are holding this object
/// with a reference to it.
///
/// Some actions possible on this interface require the process/user to have
/// the `CAP_NET_ADMIN` capability, like the root user does. This is
/// indicated by their documentation starting with "PRIVILEGED:".
#[allow(missing_copy_implementations)]
#[derive(Debug)]
pub struct CanInterface {
    if_index: c_uint,
}

impl CanInterface {
    /// Open a CAN interface by name.
    ///
    /// Similar to `open_iface`, but looks up the device by name instead of
    /// the interface index.
    pub fn open(ifname: &str) -> Result<Self, nix::Error> {
        let if_index = if_nametoindex(ifname)?;
        Ok(Self::open_iface(if_index))
    }

    /// Open a CAN interface.
    ///
    /// Creates a new `CanInterface` instance.
    ///
    /// Note that no actual "opening" or checks are performed when calling
    /// this function, nor does it test to determine if the interface with
    /// the specified index actually exists.
    pub fn open_iface(if_index: u32) -> Self {
        let if_index = if_index as c_uint;
        Self { if_index }
    }

    /// Creates an `Ifinfomsg` for this CAN interface from a buffer
    fn info_msg(&self, buf: RtBuffer<Ifla, Buffer>) -> Ifinfomsg {
        Ifinfomsg::new(
            RtAddrFamily::Unspecified,
            Arphrd::Netrom,
            self.if_index as c_int,
            IffFlags::empty(),
            IffFlags::empty(),
            buf,
        )
    }

    /// Sends an info message to the kernel.
    fn send_info_msg(msg_type: Rtm, info: Ifinfomsg, additional_flags: &[NlmF]) -> NlResult<()> {
        let mut nl = Self::open_route_socket()?;

        // prepare message
        let hdr = Nlmsghdr::new(
            None,
            msg_type,
            {
                let mut flags = NlmFFlags::new(&[NlmF::Request, NlmF::Ack]);
                for flag in additional_flags {
                    flags.set(flag);
                }
                flags
            },
            None,
            None,
            NlPayload::Payload(info),
        );
        // send the message
        Self::send_and_read_ack(&mut nl, hdr)
    }

    /// Sends a message down a netlink socket, and checks if an ACK was
    /// properly received.
    fn send_and_read_ack<T, P>(sock: &mut NlSocketHandle, msg: Nlmsghdr<T, P>) -> NlResult<()>
    where
        T: NlType + Debug,
        P: ToBytes + Debug,
    {
        sock.send(msg)?;

        // This will actually produce an Err if the response is a netlink error,
        // no need to match.
        if let Some(Nlmsghdr {
            nl_payload: NlPayload::Ack(_),
            ..
        }) = sock.recv()?
        {
            Ok(())
        } else {
            Err(NlError::NoAck)
        }
    }

    /// Opens a new netlink socket, bound to this process' PID.
    /// The function is generic to allow for usage in contexts where NlError
    /// has specific, non-default, generic parameters.
    fn open_route_socket<T, P>() -> Result<NlSocketHandle, NlError<T, P>> {
        // retrieve PID
        let pid = unistd::getpid().as_raw() as u32;

        // open and bind socket
        // groups is set to None(0), because we want no notifications
        let sock = NlSocketHandle::connect(NlFamily::Route, Some(pid), &[])?;
        Ok(sock)
    }

    // Send a netlink CAN command down to the kernel to set an attribute
    // in the link info, such as bitrate, control modes, etc
    fn set_can_param(&self, param: IflaCan, param_data: &[u8]) -> NlResult<()> {
        let info = self.info_msg({
            let mut data = Rtattr::new(None, IflaInfo::Data, Buffer::new())?;
            data.add_nested_attribute(&Rtattr::new(None, param /*as u16*/, param_data)?)?;

            let mut link_info = Rtattr::new(None, Ifla::Linkinfo, Buffer::new())?;
            link_info.add_nested_attribute(&Rtattr::new(None, IflaInfo::Kind, "can")?)?;
            link_info.add_nested_attribute(&data)?;

            let mut rtattrs = RtBuffer::new();
            rtattrs.push(link_info);
            rtattrs
        });
        Self::send_info_msg(Rtm::Newlink, info, &[])
    }

    /// Parses the info attribute to find and extratc a CAN parameter.
    #[allow(dead_code)]
    fn parse_can_param<P>(
        link_info: Rtattr<IflaInfo, Buffer>,
        param: IflaCan,
    ) -> Result<Option<P>, NlError<Rtm, Ifinfomsg>>
    where
        P: for<'a> FromBytes<'a> + Clone,
    {
        for info in link_info.get_attr_handle::<IflaInfo>()?.get_attrs() {
            if info.rta_type == IflaInfo::Data {
                for attr in info.get_attr_handle::<IflaCan>()?.get_attrs() {
                    if attr.rta_type == param {
                        return Ok(Some(attr.get_payload_as::<P>()?));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Attempt to query a CAN parameter on the interface.
    pub fn can_param<P>(&self, param: IflaCan) -> Result<Option<P>, NlError<Rtm, Ifinfomsg>>
    where
        P: for<'a> FromBytes<'a> + Clone,
    {
        let info = self.info_msg({
            let mut buffer = RtBuffer::new();
            buffer.push(Rtattr::new(None, Ifla::ExtMask, rt::EXT_FILTER_VF).unwrap());
            buffer
        });

        let hdr = Nlmsghdr::new(
            None,
            Rtm::Getlink,
            NlmFFlags::new(&[NlmF::Request]),
            None,
            None,
            NlPayload::Payload(info),
        );

        let mut nl = Self::open_route_socket()?;
        nl.send(hdr)?;

        if let Some(msg) = nl.recv::<'_, Rtm, Ifinfomsg>()? {
            if let Ok(payload) = msg.get_payload() {
                for top_attr in payload.rtattrs.iter() {
                    if top_attr.rta_type == Ifla::Linkinfo {
                        for info in top_attr.get_attr_handle::<IflaInfo>()?.get_attrs() {
                            if info.rta_type == IflaInfo::Data {
                                for attr in info.get_attr_handle::<IflaCan>()?.get_attrs() {
                                    if attr.rta_type == param {
                                        return Ok(Some(attr.get_payload_as::<P>()?));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Ok(None)
        } else {
            Err(NlError::NoAck)
        }
    }

    /// Bring down this interface.
    ///
    /// Use a netlink control socket to set the interface status to "down".
    pub fn bring_down(&self) -> NlResult<()> {
        // Specific iface down info
        let info = Ifinfomsg::down(
            RtAddrFamily::Unspecified,
            Arphrd::Netrom,
            self.if_index as c_int,
            RtBuffer::new(),
        );
        Self::send_info_msg(Rtm::Newlink, info, &[])
    }

    /// Bring up this interface
    ///
    /// Brings the interface up by settings its "up" flag enabled via netlink.
    pub fn bring_up(&self) -> NlResult<()> {
        // Specific iface up info
        let info = Ifinfomsg::up(
            RtAddrFamily::Unspecified,
            Arphrd::Netrom,
            self.if_index as c_int,
            RtBuffer::new(),
        );
        Self::send_info_msg(Rtm::Newlink, info, &[])
    }

    /// Create a virtual CAN (VCAN) interface.
    ///
    /// Useful for testing applications when a physical CAN interface and
    /// bus is not available.
    ///
    /// Note that the length of the name is capped by ```libc::IFNAMSIZ```.
    ///
    /// PRIVILEGED: This requires root privilege.
    ///
    pub fn create_vcan(name: &str, index: Option<u32>) -> NlResult<Self> {
        Self::create(name, index, "vcan")
    }

    /// Create an interface of the given kind.
    ///
    /// Note that the length of the name is capped by ```libc::IFNAMSIZ```.
    ///
    /// PRIVILEGED: This requires root privilege.
    ///
    pub fn create<I>(name: &str, index: I, kind: &str) -> NlResult<Self>
    where
        I: Into<Option<u32>>,
    {
        if name.len() > libc::IFNAMSIZ {
            return Err(NlError::Msg("Interface name too long".into()));
        }
        let index = index.into();

        let info = Ifinfomsg::new(
            RtAddrFamily::Unspecified,
            Arphrd::Netrom,
            index.unwrap_or(0) as c_int,
            IffFlags::empty(),
            IffFlags::empty(),
            {
                let mut buffer = RtBuffer::new();
                buffer.push(Rtattr::new(None, Ifla::Ifname, name)?);
                let mut linkinfo = Rtattr::new(None, Ifla::Linkinfo, Vec::<u8>::new())?;
                linkinfo.add_nested_attribute(&Rtattr::new(None, IflaInfo::Kind, kind)?)?;
                buffer.push(linkinfo);
                buffer
            },
        );
        Self::send_info_msg(Rtm::Newlink, info, &[NlmF::Create, NlmF::Excl])?;

        if let Some(if_index) = index {
            Ok(Self { if_index })
        } else {
            // Unfortunately netlink does not return the the if_index assigned to the interface.
            if let Ok(if_index) = if_nametoindex(name) {
                Ok(Self { if_index })
            } else {
                Err(NlError::Msg(
                    "Interface must have been deleted between request and this if_nametoindex"
                        .into(),
                ))
            }
        }
    }

    /// Delete the interface.
    ///
    /// PRIVILEGED: This requires root privilege.
    ///
    pub fn delete(self) -> Result<(), (Self, NlError)> {
        let info = self.info_msg(RtBuffer::new());
        match Self::send_info_msg(Rtm::Dellink, info, &[]) {
            Ok(()) => Ok(()),
            Err(err) => Err((self, err)),
        }
    }

    /// Attempt to query detailed information on the interface.
    pub fn details(&self) -> Result<InterfaceDetails, NlError<Rtm, Ifinfomsg>> {
        let info = self.info_msg({
            let mut buffer = RtBuffer::new();
            buffer.push(Rtattr::new(None, Ifla::ExtMask, rt::EXT_FILTER_VF).unwrap());
            buffer
        });

        let mut nl = Self::open_route_socket()?;

        let hdr = Nlmsghdr::new(
            None,
            Rtm::Getlink,
            NlmFFlags::new(&[NlmF::Request]),
            None,
            None,
            NlPayload::Payload(info),
        );
        nl.send(hdr)?;

        match nl.recv::<'_, Rtm, Ifinfomsg>()? {
            Some(msg_hdr) => {
                let mut info = InterfaceDetails::new(self.if_index);

                if let Ok(payload) = msg_hdr.get_payload() {
                    info.is_up = payload.ifi_flags.contains(&Iff::Up);

                    for attr in payload.rtattrs.iter() {
                        match attr.rta_type {
                            Ifla::Ifname => {
                                if let Ok(string) =
                                    CString::from_vec_with_nul(Vec::from(attr.rta_payload.as_ref()))
                                {
                                    if let Ok(string) = string.into_string() {
                                        info.name = Some(string);
                                    }
                                }
                            }
                            Ifla::Mtu => {
                                if attr.rta_payload.len() == 4 {
                                    let mut bytes = [0u8; 4];
                                    for (index, byte) in
                                        attr.rta_payload.as_ref().iter().enumerate()
                                    {
                                        bytes[index] = *byte;
                                    }

                                    info.mtu = Mtu::try_from(u32::from_ne_bytes(bytes)).ok();
                                }
                            }
                            Ifla::Linkinfo => {
                                for info_attr in attr.get_attr_handle::<IflaInfo>()?.get_attrs() {
                                    if info_attr.rta_type == IflaInfo::Data {
                                        for attr in
                                            info_attr.get_attr_handle::<IflaCan>()?.get_attrs()
                                        {
                                            match attr.rta_type {
                                                IflaCan::BitTiming => {
                                                    info.bit_timing = Some(
                                                        attr.get_payload_as::<rt::can_bittiming>()?,
                                                    )
                                                }
                                                _ => (),
                                            }
                                        }
                                    }
                                }
                            }
                            _ => (),
                        }
                    }
                }

                Ok(info)
            }
            None => Err(NlError::NoAck),
        }
    }

    /// Set the MTU of this interface.
    ///
    /// PRIVILEGED: This requires root privilege.
    ///
    pub fn set_mtu(&self, mtu: Mtu) -> NlResult<()> {
        let mtu = mtu as u32;
        let info = self.info_msg({
            let mut buffer = RtBuffer::new();
            buffer.push(Rtattr::new(None, Ifla::Mtu, &mtu.to_ne_bytes()[..])?);
            buffer
        });
        Self::send_info_msg(Rtm::Newlink, info, &[])
    }

    /// Gets the bit timing data for the interface
    pub fn bit_timing(&self) -> Result<Option<rt::can_bittiming>, NlError<Rtm, Ifinfomsg>> {
        self.can_param::<rt::can_bittiming>(IflaCan::BitTiming)
    }

    /// Set the bitrate and, optionally, sample point of this interface.
    ///
    /// The bitrate can *not* be changed if the interface is UP. It is
    /// specified in Hz (bps) while the sample point is given in tenths
    /// of a percent/
    ///
    /// PRIVILEGED: This requires root privilege.
    ///
    pub fn set_bitrate<P>(&self, bitrate: u32, sample_point: P) -> NlResult<()>
    where
        P: Into<Option<u32>>,
    {
        let sample_point: u32 = sample_point.into().unwrap_or(0);

        debug_assert!(
            0 < bitrate && bitrate <= 1000000,
            "Bitrate must be within 1..=1000000, received {}.",
            bitrate
        );
        debug_assert!(
            sample_point < 1000,
            "Sample point must be within 0..1000, received {}.",
            sample_point
        );

        let timing = rt::can_bittiming {
            bitrate,
            sample_point,
            ..rt::can_bittiming::default()
        };

        self.set_can_param(IflaCan::BitTiming, as_bytes(&timing))
    }

    /// Set the data bitrate and, optionally, data sample point of this
    /// interface.
    ///
    /// This only applies to interfaces in FD mode.
    ///
    /// The data bitrate can *not* be changed if the interface is UP. It is
    /// specified in Hz (bps) while the sample point is given in tenths
    /// of a percent/
    ///
    /// PRIVILEGED: This requires root privilege.
    ///
    pub fn set_data_bitrate<P>(&self, bitrate: u32, sample_point: P) -> NlResult<()>
    where
        P: Into<Option<u32>>,
    {
        let sample_point: u32 = sample_point.into().unwrap_or(0);

        let timing = rt::can_bittiming {
            bitrate,
            sample_point,
            ..rt::can_bittiming::default()
        };

        self.set_can_param(IflaCan::DataBitTiming, as_bytes(&timing))
    }

    /// Set the full control mode (bit) collection.
    #[deprecated(since = "3.2.0", note = "Use `set_ctrlmodes` instead")]
    pub fn set_full_ctrlmode(&self, ctrlmode: rt::can_ctrlmode) -> NlResult<()> {
        self.set_can_param(IflaCan::CtrlMode, as_bytes(&ctrlmode))
    }

    /// Set the full control mode (bit) collection.
    pub fn set_ctrlmodes<M>(&self, ctrlmode: M) -> NlResult<()>
    where
        M: Into<CanCtrlModes>,
    {
        let modes = ctrlmode.into();
        let modes: rt::can_ctrlmode = modes.into();
        self.set_can_param(IflaCan::CtrlMode, as_bytes(&modes))
    }

    /// Set or clear an individual control mode parameter.
    pub fn set_ctrlmode(&self, mode: CanCtrlMode, on: bool) -> NlResult<()> {
        self.set_ctrlmodes(CanCtrlModes::from_mode(mode, on))
    }

    /// Set the automatic restart milliseconds of the interface
    ///
    /// PRIVILEGED: This requires root privilege.
    ///
    pub fn set_restart_ms(&self, restart_ms: u32) -> NlResult<()> {
        self.set_can_param(IflaCan::RestartMs, &restart_ms.to_ne_bytes())
    }

    /// Manually restart the interface.
    ///
    /// Note that a manual restart if only permitted if automatic restart is
    /// disabled and the device is in the bus-off state.
    /// See: linux/drivers/net/can/dev/dev.c
    ///
    /// PRIVILEGED: This requires root privilege.
    ///
    /// Common Errors:
    ///     EINVAL - The interface is down or automatic restarts are enabled
    ///     EBUSY - The interface is not in a bus-off state
    ///
    pub fn restart(&self) -> NlResult<()> {
        // Note: The linux code shows the data type to be u32, but never
        // appears to access the value sent. iproute2 sends a 1, so we do
        // too!
        // See: linux/drivers/net/can/dev/netlink.c
        let restart_data: u32 = 1;
        self.set_can_param(IflaCan::Restart, &restart_data.to_ne_bytes())
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn test_as_bytes() {
        let bitrate = 500000;
        let sample_point = 750;
        let timing = rt::can_bittiming {
            bitrate,
            sample_point,
            ..rt::can_bittiming::default()
        };

        assert_eq!(
            unsafe {
                std::slice::from_raw_parts::<'_, u8>(
                    &timing as *const _ as *const u8,
                    std::mem::size_of::<rt::can_bittiming>(),
                )
            },
            as_bytes(&timing)
        );
    }
}

#[cfg(test)]
#[cfg(feature = "netlink_tests")]
pub mod tests {
    use std::ops::Deref;

    use serial_test::serial;

    use super::*;

    /// RAII-style helper to create and clean-up a specific vcan interface for a single test.
    /// Using drop here ensures that the interface always gets cleaned up
    /// (although a restart would also remove it).
    ///
    /// Intended for use (ONLY) in tests as follows:
    /// ```
    /// #[test]
    /// fn my_test() {
    ///     let interface = TemporaryInterface::new("my_test").unwrap();
    ///     // use the interface..
    /// }
    /// ```
    /// Please note that there is a limit to the length of interface names,
    /// namely 16 characters on Linux.
    pub struct TemporaryInterface {
        interface: CanInterface,
    }

    impl TemporaryInterface {
        #[allow(unused)]
        pub fn new(name: &str) -> NlResult<Self> {
            Ok(Self {
                interface: CanInterface::create_vcan(name, None)?,
            })
        }
    }

    impl Drop for TemporaryInterface {
        fn drop(&mut self) {
            assert!(CanInterface::open_iface(self.interface.if_index)
                .delete()
                .is_ok());
        }
    }

    impl Deref for TemporaryInterface {
        type Target = CanInterface;

        fn deref(&self) -> &Self::Target {
            &self.interface
        }
    }

    #[cfg(feature = "netlink_tests")]
    #[test]
    #[serial]
    fn up_down() {
        let interface = TemporaryInterface::new("up_down").unwrap();

        assert!(interface.bring_up().is_ok());
        assert!(interface.details().unwrap().is_up);

        assert!(interface.bring_down().is_ok());
        assert!(!interface.details().unwrap().is_up);
    }

    #[cfg(feature = "netlink_tests")]
    #[test]
    #[serial]
    fn details() {
        let interface = TemporaryInterface::new("info").unwrap();
        let details = interface.details().unwrap();
        assert_eq!("info", details.name.unwrap());
        assert!(details.mtu.is_some());
        assert!(!details.is_up);
    }

    #[cfg(feature = "netlink_tests")]
    #[test]
    #[serial]
    fn mtu() {
        let interface = TemporaryInterface::new("mtu").unwrap();

        assert!(interface.set_mtu(Mtu::Fd).is_ok());
        assert_eq!(Mtu::Fd, interface.details().unwrap().mtu.unwrap());

        assert!(interface.set_mtu(Mtu::Standard).is_ok());
        assert_eq!(Mtu::Standard, interface.details().unwrap().mtu.unwrap());
    }
}