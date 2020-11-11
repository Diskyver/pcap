//! pcap is a packet capture library available on Linux, Windows and Mac. This
//! crate supports creating and configuring capture contexts, sniffing packets,
//! sending packets to interfaces, listing devices, and recording packet captures
//! to pcap-format dump files.
//!
//! # Capturing packets
//! The easiest way to open an active capture handle and begin sniffing is to
//! use `.open()` on a `Device`. You can obtain the "default" device using
//! `Device::lookup()`, or you can obtain the device(s) you need via `Device::list()`.
//!
//! ```ignore
//! use pcap::Device;
//!
//! fn main() {
//!     let mut cap = Device::lookup().unwrap().open().unwrap();
//!
//!     while let Ok(packet) = cap.next() {
//!         println!("received packet! {:?}", packet);
//!     }
//! }
//! ```
//!
//! `Capture`'s `.next()` will produce a `Packet` which can be dereferenced to access the
//! `&[u8]` packet contents.
//!
//! # Custom configuration
//!
//! You may want to configure the `timeout`, `snaplen` or other parameters for the capture
//! handle. In this case, use `Capture::from_device()` to obtain a `Capture<Inactive>`, and
//! proceed to configure the capture handle. When you're finished, run `.open()` on it to
//! turn it into a `Capture<Active>`.
//!
//! ```ignore
//! use pcap::{Device,Capture};
//!
//! fn main() {
//!     let main_device = Device::lookup().unwrap();
//!     let mut cap = Capture::from_device(main_device).unwrap()
//!                       .promisc(true)
//!                       .snaplen(5000)
//!                       .open().unwrap();
//!
//!     while let Ok(packet) = cap.next() {
//!         println!("received packet! {:?}", packet);
//!     }
//! }
//! ```

use unique::Unique;

use std::borrow::Borrow;
use std::marker::PhantomData;
use std::ptr;
use std::ffi::{self, CString, CStr};
use std::path::Path;
use std::slice;
use std::ops::Deref;
use std::mem;
use std::fmt;
#[cfg(feature = "capture-stream")]
use std::io;
#[cfg(not(windows))]
use std::os::unix::io::{RawFd, AsRawFd};

use self::Error::*;

mod raw;
mod unique;
#[cfg(feature = "capture-stream")]
pub mod stream;

/// An error received from pcap
#[derive(Debug, PartialEq)]
pub enum Error {
    MalformedError(std::str::Utf8Error),
    InvalidString,
    PcapError(String),
    InvalidLinktype,
    TimeoutExpired,
    NoMorePackets,
    NonNonBlock,
    InsufficientMemory,
    InvalidInputString,
    IoError(std::io::ErrorKind),
    #[cfg(not(windows))]
    InvalidRawFd,
}

impl Error {
    fn new(ptr: *const libc::c_char) -> Error {
        match cstr_to_string(ptr) {
            Err(e) => e as Error,
            Ok(string) => PcapError(string.unwrap_or_default()),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            MalformedError(ref e) => write!(f, "libpcap returned invalid UTF-8: {}", e),
            InvalidString => write!(f, "libpcap returned a null string"),
            PcapError(ref e) => write!(f, "libpcap error: {}", e),
            InvalidLinktype => write!(f, "invalid or unknown linktype"),
            TimeoutExpired => write!(f, "timeout expired while reading from a live capture"),
            NonNonBlock => write!(f, "must be in non-blocking mode to function"),
            NoMorePackets => write!(f, "no more packets to read from the file"),
            InsufficientMemory => write!(f, "insufficient memory"),
            InvalidInputString => write!(f, "invalid input string (internal null)"),
            IoError(ref e) => write!(f, "io error occurred: {:?}", e),
            #[cfg(not(windows))]
            InvalidRawFd => write!(f, "invalid raw file descriptor provided"),
        }
    }
}

impl std::error::Error for Error {
    fn description(&self) -> &str {
        match *self {
            MalformedError(..) => "libpcap returned invalid UTF-8",
            PcapError(..) => "libpcap FFI error",
            InvalidString => "libpcap returned a null string",
            InvalidLinktype => "invalid or unknown linktype",
            TimeoutExpired => "timeout expired while reading from a live capture",
            NonNonBlock => "must be in non-blocking mode to function",
            NoMorePackets => "no more packets to read from the file",
            InsufficientMemory => "insufficient memory",
            InvalidInputString => "invalid input string (internal null)",
            IoError(..) => "io error occurred",
            #[cfg(not(windows))]
            InvalidRawFd => "invalid raw file descriptor provided",
        }
    }

    fn cause(&self) -> Option<&dyn std::error::Error> {
        match *self {
            MalformedError(ref e) => Some(e),
            _ => None,
        }
    }
}

impl From<ffi::NulError> for Error {
    fn from(_: ffi::NulError) -> Error {
        InvalidInputString
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(obj: std::str::Utf8Error) -> Error {
        MalformedError(obj)
    }
}

impl From<std::io::Error> for Error {
    fn from(obj: std::io::Error) -> Error {
        IoError(obj.kind())
    }
}

impl From<std::io::ErrorKind> for Error {
    fn from(obj: std::io::ErrorKind) -> Error {
        IoError(obj)
    }
}

#[derive(Debug, Clone)]
/// A network device name and (potentially) pcap's description of it.
pub struct Device {
    pub name: String,
    pub desc: Option<String>,
}

impl Device {
    fn new(name: String, desc: Option<String>) -> Device {
        Device { name, desc }
    }

    /// Opens a `Capture<Active>` on this device.
    pub fn open(self) -> Result<Capture<Active>, Error> {
        Capture::from_device(self)?.open()
    }

    /// Returns the default Device suitable for captures according to pcap_lookupdev,
    /// or an error from pcap.
    pub fn lookup() -> Result<Device, Error> {
        with_errbuf(|err| unsafe {
            cstr_to_string(raw::pcap_lookupdev(err))
                ?
                .map(|name| Device::new(name, None))
                .ok_or_else(|| Error::new(err))
        })
    }

    /// Returns a vector of `Device`s known by pcap via pcap_findalldevs.
    pub fn list() -> Result<Vec<Device>, Error> {
        with_errbuf(|err| unsafe {
            let mut dev_buf: *mut raw::pcap_if_t = ptr::null_mut();
            if raw::pcap_findalldevs(&mut dev_buf, err) != 0 {
                return Err(Error::new(err));
            }
            let result = (|| {
                let mut devices = vec![];
                let mut cur = dev_buf;
                while !cur.is_null() {
                    let dev = &*cur;
                    devices.push(Device::new(cstr_to_string(dev.name)?.ok_or(InvalidString)?,
                                             cstr_to_string(dev.description)?));
                    cur = dev.next;
                }
                Ok(devices)
            })();
            raw::pcap_freealldevs(dev_buf);
            result
        })
    }
}

impl<'a> Into<Device> for &'a str {
    fn into(self) -> Device {
        Device::new(self.into(), None)
    }
}

/// This is a datalink link type.
///
/// As an example, `Linktype(1)` is ethernet. A full list of linktypes is available
/// [here](http://www.tcpdump.org/linktypes.html). The const bellow are not exhaustive.
/// ```rust
/// use pcap::Linktype;
///
/// let lt = Linktype(1);
/// assert_eq!(Linktype::ETHERNET, lt);
/// ```
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Linktype(pub i32);

impl Linktype {
    /// Gets the name of the link type, such as EN10MB
    pub fn get_name(&self) -> Result<String, Error> {
        cstr_to_string(unsafe { raw::pcap_datalink_val_to_name(self.0) })?.ok_or(InvalidLinktype)
    }

    /// Gets the description of a link type.
    pub fn get_description(&self) -> Result<String, Error> {
        cstr_to_string(unsafe { raw::pcap_datalink_val_to_description(self.0) })?
            .ok_or(InvalidLinktype)
    }

    pub const NULL: Self = Self(0);
    pub const ETHERNET: Self = Self(1);
    pub const AX25: Self = Self(3);
    pub const IEEE802_5: Self = Self(6);
    pub const ARCNET_BSD: Self = Self(7);
    pub const SLIP: Self = Self(8);
    pub const PPP: Self = Self(9);
    pub const FDDI: Self = Self(10);
    pub const PPP_HDLC: Self = Self(50);
    pub const PPP_ETHER: Self = Self(51);
    pub const ATM_RFC1483: Self = Self(100);
    pub const RAW: Self = Self(101);
    pub const C_HDLC: Self = Self(104);
    pub const IEEE802_11: Self = Self(105);
    pub const FRELAY: Self = Self(107);
    pub const LOOP: Self = Self(108);
    pub const LINUX_SLL: Self = Self(113);
    pub const LTALK: Self = Self(114);
    pub const PFLOG: Self = Self(117);
    pub const IEEE802_11_PRISM: Self = Self(119);
    pub const IP_OVER_FC: Self = Self(122);
    pub const SUNATM: Self = Self(123);
    pub const IEEE802_11_RADIOTAP: Self = Self(127);
    pub const ARCNET_LINUX: Self = Self(129);
    pub const APPLE_IP_OVER_IEEE1394: Self = Self(138);
    pub const MTP2_WITH_PHDR: Self = Self(139);
    pub const MTP2: Self = Self(140);
    pub const MTP3: Self = Self(141);
    pub const SCCP: Self = Self(142);
    pub const DOCSIS: Self = Self(143);
    pub const LINUX_IRDA: Self = Self(144);
    pub const USER0: Self = Self(147);
    pub const USER1: Self = Self(148);
    pub const USER2: Self = Self(149);
    pub const USER3: Self = Self(150);
    pub const USER4: Self = Self(151);
    pub const USER5: Self = Self(152);
    pub const USER6: Self = Self(153);
    pub const USER7: Self = Self(154);
    pub const USER8: Self = Self(155);
    pub const USER9: Self = Self(156);
    pub const USER10: Self = Self(157);
    pub const USER11: Self = Self(158);
    pub const USER12: Self = Self(159);
    pub const USER13: Self = Self(160);
    pub const USER14: Self = Self(161);
    pub const USER15: Self = Self(162);
    pub const IEEE802_11_AVS: Self = Self(163);
    pub const BACNET_MS_TP: Self = Self(165);
    pub const PPP_PPPD: Self = Self(166);
    pub const GPRS_LLC: Self = Self(169);
    pub const GPF_T: Self = Self(170);
    pub const GPF_F: Self = Self(171);
    pub const LINUX_LAPD: Self = Self(177);
    pub const MFR: Self = Self(182);
    pub const BLUETOOTH_HCI_H4: Self = Self(187);
    pub const USB_LINUX: Self = Self(189);
    pub const PPI: Self = Self(192);
    pub const IEEE802_15_4_WITHFCS: Self = Self(195);
    pub const SITA: Self = Self(196);
    pub const ERF: Self = Self(197);
    pub const BLUETOOTH_HCI_H4_WITH_PHDR: Self = Self(201);
    pub const AX25_KISS: Self = Self(202);
    pub const LAPD: Self = Self(203);
    pub const PPP_WITH_DIR: Self = Self(204);
    pub const C_HDLC_WITH_DIR: Self = Self(205);
    pub const FRELAY_WITH_DIR: Self = Self(206);
    pub const LAPB_WITH_DIR: Self = Self(207);
    pub const IPMB_LINUX: Self = Self(209);
    pub const IEEE802_15_4_NONASK_PHY: Self = Self(215);
    pub const USB_LINUX_MMAPPED: Self = Self(220);
    pub const FC_2: Self = Self(224);
    pub const FC_2_WITH_FRAME_DELIMS: Self = Self(225);
    pub const IPNET: Self = Self(226);
    pub const CAN_SOCKETCAN: Self = Self(227);
    pub const IPV4: Self = Self(228);
    pub const IPV6: Self = Self(229);
    pub const IEEE802_15_4_NOFCS: Self = Self(230);
    pub const DBUS: Self = Self(231);
    pub const DVB_CI: Self = Self(235);
    pub const MUX27010: Self = Self(236);
    pub const STANAG_5066_D_PDU: Self = Self(237);
    pub const NFLOG: Self = Self(239);
    pub const NETANALYZER: Self = Self(240);
    pub const NETANALYZER_TRANSPARENT: Self = Self(241);
    pub const IPOIB: Self = Self(242);
    pub const MPEG_2_TS: Self = Self(243);
    pub const NG40: Self = Self(244);
    pub const NFC_LLCP: Self = Self(245);
    pub const INFINIBAND: Self = Self(247);
    pub const SCTP: Self = Self(248);
    pub const USBPCAP: Self = Self(249);
    pub const RTAC_SERIAL: Self = Self(250);
    pub const BLUETOOTH_LE_LL: Self = Self(251);
    pub const NETLINK: Self = Self(253);
    pub const BLUETOOTH_LINUX_MONITOR: Self = Self(254);
    pub const BLUETOOTH_BREDR_BB: Self = Self(255);
    pub const BLUETOOTH_LE_LL_WITH_PHDR: Self = Self(256);
    pub const PROFIBUS_DL: Self = Self(257);
    pub const PKTAP: Self = Self(258);
    pub const EPON: Self = Self(259);
    pub const IPMI_HPM_2: Self = Self(260);
    pub const ZWAVE_R1_R2: Self = Self(261);
    pub const ZWAVE_R3: Self = Self(262);
    pub const WATTSTOPPER_DLM: Self = Self(263);
    pub const ISO_14443: Self = Self(264);
    pub const RDS: Self = Self(265);
    pub const USB_DARWIN: Self = Self(266);
    pub const SDLC: Self = Self(268);
    pub const LORATAP: Self = Self(270);
    pub const VSOCK: Self = Self(271);
    pub const NORDIC_BLE: Self = Self(272);
    pub const DOCSIS31_XRA31: Self = Self(273);
    pub const ETHERNET_MPACKET: Self = Self(274);
    pub const DISPLAYPORT_AUX: Self = Self(275);
    pub const LINUX_SLL2: Self = Self(276);
    pub const OPENVIZSLA: Self = Self(278);
    pub const EBHSCR: Self = Self(279);
    pub const VPP_DISPATCH: Self = Self(280);
    pub const DSA_TAG_BRCM: Self = Self(281);
    pub const DSA_TAG_BRCM_PREPEND: Self = Self(282);
    pub const IEEE802_15_4_TAP: Self = Self(283);
    pub const DSA_TAG_DSA: Self = Self(284);
    pub const DSA_TAG_EDSA: Self = Self(285);
    pub const ELEE: Self = Self(286);
    pub const Z_WAVE_SERIAL: Self = Self(287);
    pub const USB_2_0: Self = Self(288);
    pub const ATSC_ALP: Self = Self(289);
}

/// Represents a packet returned from pcap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet<'a> {
    pub header: &'a PacketHeader,
    pub data: &'a [u8],
}

impl<'a> Packet<'a> {
    #[doc(hidden)]
    pub fn new(header: &'a PacketHeader, data: &'a [u8]) -> Packet<'a> {
        Packet { header, data }
    }
}

impl<'b> Deref for Packet<'b> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.data
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
/// Represents a packet header provided by pcap, including the timeval, caplen and len.
pub struct PacketHeader {
    pub ts: libc::timeval,
    pub caplen: u32,
    pub len: u32,
}

impl fmt::Debug for PacketHeader {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f,
               "PacketHeader {{ ts: {}.{:06}, caplen: {}, len: {} }}",
               self.ts.tv_sec,
               self.ts.tv_usec,
               self.caplen,
               self.len)
    }
}

impl PartialEq for PacketHeader {
    fn eq(&self, rhs: &PacketHeader) -> bool {
        self.ts.tv_sec == rhs.ts.tv_sec && self.ts.tv_usec == rhs.ts.tv_usec &&
            self.caplen == rhs.caplen && self.len == rhs.len
    }
}

impl Eq for PacketHeader {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub received: u32,
    pub dropped: u32,
    pub if_dropped: u32,
}

impl Stat {
    fn new(received: u32, dropped: u32, if_dropped: u32) -> Stat {
        Stat { received, dropped, if_dropped }
    }
}

#[repr(u32)]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Precision {
    Micro = 0,
    Nano = 1,
}

/// Phantom type representing an inactive capture handle.
pub enum Inactive {}

/// Phantom type representing an active capture handle.
pub enum Active {}

/// Phantom type representing an offline capture handle, from a pcap dump file.
/// Implements `Activated` because it behaves nearly the same as a live handle.
pub enum Offline {}

/// Phantom type representing a dead capture handle.  This can be use to create
/// new save files that are not generated from an active capture.
/// Implements `Activated` because it behaves nearly the same as a live handle.
pub enum Dead {}

pub unsafe trait Activated: State {}

unsafe impl Activated for Active {}

unsafe impl Activated for Offline {}

unsafe impl Activated for Dead {}

/// `Capture`s can be in different states at different times, and in these states they
/// may or may not have particular capabilities. This trait is implemented by phantom
/// types which allows us to punt these invariants to the type system to avoid runtime
/// errors.
pub unsafe trait State {}

unsafe impl State for Inactive {}

unsafe impl State for Active {}

unsafe impl State for Offline {}

unsafe impl State for Dead {}

/// This is a pcap capture handle which is an abstraction over the `pcap_t` provided by pcap.
/// There are many ways to instantiate and interact with a pcap handle, so phantom types are
/// used to express these behaviors.
///
/// **`Capture<Inactive>`** is created via `Capture::from_device()`. This handle is inactive,
/// so you cannot (yet) obtain packets from it. However, you can configure things like the
/// buffer size, snaplen, timeout, and promiscuity before you activate it.
///
/// **`Capture<Active>`** is created by calling `.open()` on a `Capture<Inactive>`. This
/// activates the capture handle, allowing you to get packets with `.next()` or apply filters
/// with `.filter()`.
///
/// **`Capture<Offline>`** is created via `Capture::from_file()`. This allows you to read a
/// pcap format dump file as if you were opening an interface -- very useful for testing or
/// analysis.
///
/// **`Capture<Dead>`** is created via `Capture::dead()`. This allows you to create a pcap
/// format dump file without needing an active capture.
///
/// # Example:
///
/// ```ignore
/// let cap = Capture::from_device(Device::lookup().unwrap()) // open the "default" interface
///               .unwrap() // assume the device exists and we are authorized to open it
///               .open() // activate the handle
///               .unwrap(); // assume activation worked
///
/// while let Ok(packet) = cap.next() {
///     println!("received packet! {:?}", packet);
/// }
/// ```
pub struct Capture<T: State + ? Sized> {
    nonblock: bool,
    handle: Unique<raw::pcap_t>,
    _marker: PhantomData<T>,
}

impl<T: State + ? Sized> Capture<T> {
    fn new(handle: *mut raw::pcap_t) -> Capture<T> {
        unsafe {
            Capture {
                nonblock: false,
                handle: Unique::new(handle),
                _marker: PhantomData,
            }
        }
    }

    fn new_raw<F>(path: Option<&str>, func: F) -> Result<Capture<T>, Error>
    where F: FnOnce(*const libc::c_char, *mut libc::c_char) -> *mut raw::pcap_t
    {
        with_errbuf(|err| {
            let handle = match path {
                None => func(ptr::null(), err),
                Some(path) => {
                    let path = CString::new(path)?;
                    func(path.as_ptr(), err)
                }
            };
            unsafe { handle.as_mut() }.map(|h| Capture::new(h)).ok_or_else(|| Error::new(err))
        })
    }

    /// Set the minumum amount of data received by the kernel in a single call.
    ///
    /// Note that this value is set to 0 when the capture is set to immediate mode. You should not
    /// call `min_to_copy` on captures in immediate mode if you want them to stay in immediate mode.
    #[cfg(windows)]
    pub fn min_to_copy(self, to: i32) -> Capture<T> {
        unsafe { raw::pcap_setmintocopy(*self.handle, to as _); }
        self
    }

    #[inline]
    fn check_err(&self, success: bool) -> Result<(), Error> {
        if success {
            Ok(())
        } else {
            Err(Error::new(unsafe { raw::pcap_geterr(*self.handle) }))
        }
    }
}

impl Capture<Offline> {
    /// Opens an offline capture handle from a pcap dump file, given a path.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Capture<Offline>, Error> {
        Capture::new_raw(path.as_ref().to_str(),
                         |path, err| unsafe { raw::pcap_open_offline(path, err) })
    }

    /// Opens an offline capture handle from a pcap dump file, given a path.
    /// Takes an additional precision argument specifying the time stamp precision desired.
    #[cfg(libpcap_1_5_0)]
    pub fn from_file_with_precision<P: AsRef<Path>>(path: P, precision: Precision) -> Result<Capture<Offline>, Error> {
        Capture::new_raw(path.as_ref().to_str(), |path, err| unsafe {
            raw::pcap_open_offline_with_tstamp_precision(path, precision as _, err)
        })
    }

    /// Opens an offline capture handle from a pcap dump file, given a file descriptor.
    #[cfg(not(windows))]
    pub fn from_raw_fd(fd: RawFd) -> Result<Capture<Offline>, Error> {
        open_raw_fd(fd, b'r')
            .and_then(|file| Capture::new_raw(None, |_, err| unsafe {
                raw::pcap_fopen_offline(file, err)
            }))
    }

    /// Opens an offline capture handle from a pcap dump file, given a file descriptor.
    /// Takes an additional precision argument specifying the time stamp precision desired.
    #[cfg(all(not(windows), libpcap_1_5_0))]
    pub fn from_raw_fd_with_precision(fd: RawFd, precision: Precision) -> Result<Capture<Offline>, Error> {
        open_raw_fd(fd, b'r')
            .and_then(|file| Capture::new_raw(None, |_, err| unsafe {
                raw::pcap_fopen_offline_with_tstamp_precision(file, precision as _, err)
            }))
    }
}

#[repr(i32)]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TimestampType {
    Host = 0,
    HostLowPrec = 1,
    HostHighPrec = 2,
    Adapter = 3,
    AdapterUnsynced = 4,
}

#[deprecated(note = "Renamed to TimestampType")]
pub type TstampType = TimestampType;

#[repr(u32)]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Direction {
    InOut = raw::PCAP_D_INOUT,
    In = raw::PCAP_D_IN,
    Out = raw::PCAP_D_OUT,
}

impl Capture<Inactive> {
    /// Opens a capture handle for a device. You can pass a `Device` or an `&str` device
    /// name here. The handle is inactive, but can be activated via `.open()`.
    ///
    /// # Example
    /// ```
    /// use pcap::*;
    ///
    /// // Usage 1: Capture from a single owned device
    /// let dev: Device = pcap::Device::lookup().unwrap();
    /// let cap1 = Capture::from_device(dev);
    ///
    /// // Usage 2: Capture from an element of device list.
    /// let list: Vec<Device> = pcap::Device::list().unwrap();
    /// let cap2 = Capture::from_device(list[0].clone());
    ///
    /// // Usage 3: Capture from `&str` device name
    /// let cap3 = Capture::from_device("eth0");
    /// ```
    pub fn from_device<D: Into<Device>>(device: D) -> Result<Capture<Inactive>, Error> {
        let device: Device = device.into();
        Capture::new_raw(Some(&device.name),
                         |name, err| unsafe { raw::pcap_create(name, err) })
    }

    /// Activates an inactive capture created from `Capture::from_device()` or returns
    /// an error.
    pub fn open(self) -> Result<Capture<Active>, Error> {
        unsafe {
            self.check_err(raw::pcap_activate(*self.handle) == 0)?;
            Ok(mem::transmute(self))
        }
    }

    /// Set the read timeout for the Capture. By default, this is 0, so it will block
    /// indefinitely.
    pub fn timeout(self, ms: i32) -> Capture<Inactive> {
        unsafe { raw::pcap_set_timeout(*self.handle, ms) };
        self
    }

    /// Set the time stamp type to be used by a capture device.
    #[cfg(libpcap_1_2_1)]
    pub fn tstamp_type(self, tstamp_type: TimestampType) -> Capture<Inactive> {
        unsafe { raw::pcap_set_tstamp_type(*self.handle, tstamp_type as _) };
        self
    }

    /// Set promiscuous mode on or off. By default, this is off.
    pub fn promisc(self, to: bool) -> Capture<Inactive> {
        unsafe { raw::pcap_set_promisc(*self.handle, to as _) };
        self
    }

    /// Set immediate mode on or off. By default, this is off.
    ///
    /// Note that in WinPcap immediate mode is set by passing a 0 argument to `min_to_copy`.
    /// Immediate mode will be unset if `min_to_copy` is later called with a non-zero argument.
    /// Immediate mode is unset by resetting `min_to_copy` to the WinPcap default possibly changing
    /// a previously set value. When using `min_to_copy`, it is best to avoid `immediate_mode`.
    #[cfg(any(libpcap_1_5_0, windows))]
    pub fn immediate_mode(self, to: bool) -> Capture<Inactive> {
        // Prior to 1.5.0 when `pcap_set_immediate_mode` was introduced, the necessary steps to set
        // immediate mode were more complicated, depended on the OS, and in some configurations had
        // to be set on an active capture. See
        // https://www.tcpdump.org/manpages/pcap_set_immediate_mode.3pcap.html. Since we do not
        // expect pre-1.5.0 version on unix systems in the wild, we simply ignore those cases.
        #[cfg(libpcap_1_5_0)]
        unsafe { raw::pcap_set_immediate_mode(*self.handle, to as _) };

        // In WinPcap we use `pcap_setmintocopy` as it does not have `pcap_set_immediate_mode`.
        #[cfg(all(windows, not(libpcap_1_5_0)))]
        unsafe { raw::pcap_setmintocopy(*self.handle, if to { 0 } else { raw::WINPCAP_MINTOCOPY_DEFAULT }) };

        self
    }

    /// Set rfmon mode on or off. The default is maintained by pcap.
    #[cfg(not(windows))]
    pub fn rfmon(self, to: bool) -> Capture<Inactive> {
        unsafe { raw::pcap_set_rfmon(*self.handle, to as _) };
        self
    }

    /// Set the buffer size for incoming packet data.
    ///
    /// The default is 1000000. This should always be larger than the snaplen.
    pub fn buffer_size(self, to: i32) -> Capture<Inactive> {
        unsafe { raw::pcap_set_buffer_size(*self.handle, to) };
        self
    }

    /// Set the time stamp precision returned in captures.
    #[cfg(libpcap_1_5_0)]
    pub fn precision(self, precision: Precision) -> Capture<Inactive> {
        unsafe { raw::pcap_set_tstamp_precision(*self.handle, precision as _) };
        self
    }

    /// Set the snaplen size (the maximum length of a packet captured into the buffer).
    /// Useful if you only want certain headers, but not the entire packet.
    ///
    /// The default is 65535.
    pub fn snaplen(self, to: i32) -> Capture<Inactive> {
        unsafe { raw::pcap_set_snaplen(*self.handle, to) };
        self
    }
}

///# Activated captures include `Capture<Active>` and `Capture<Offline>`.
impl<T: Activated + ? Sized> Capture<T> {
    /// List the datalink types that this captured device supports.
    pub fn list_datalinks(&self) -> Result<Vec<Linktype>, Error> {
        unsafe {
            let mut links: *mut i32 = ptr::null_mut();
            let num = raw::pcap_list_datalinks(*self.handle, &mut links);
            let mut vec = vec![];
            if num > 0 {
                vec.extend(slice::from_raw_parts(links, num as _).iter().cloned().map(Linktype))
            }
            raw::pcap_free_datalinks(links);
            self.check_err(num > 0).and(Ok(vec))
        }
    }

    /// Set the datalink type for the current capture handle.
    pub fn set_datalink(&mut self, linktype: Linktype) -> Result<(), Error> {
        self.check_err(unsafe { raw::pcap_set_datalink(*self.handle, linktype.0) == 0 })
    }

    /// Get the current datalink type for this capture handle.
    pub fn get_datalink(&self) -> Linktype {
        unsafe { Linktype(raw::pcap_datalink(*self.handle)) }
    }

    /// Create a `Savefile` context for recording captured packets using this `Capture`'s
    /// configurations.
    pub fn savefile<P: AsRef<Path>>(&self, path: P) -> Result<Savefile, Error> {
        let name = CString::new(path.as_ref().to_str().unwrap())?;
        let handle = unsafe { raw::pcap_dump_open(*self.handle, name.as_ptr()) };
        self.check_err(!handle.is_null()).map(|_| Savefile::new(handle))
    }

    /// Create a `Savefile` context for recording captured packets using this `Capture`'s
    /// configurations. The output is written to a raw file descriptor which is opened
    /// in `"w"` mode.
    #[cfg(not(windows))]
    pub fn savefile_raw_fd(&self, fd: RawFd) -> Result<Savefile, Error> {
        open_raw_fd(fd, b'w')
            .and_then(|file| {
                let handle = unsafe { raw::pcap_dump_fopen(*self.handle, file) };
                self.check_err(!handle.is_null()).map(|_| Savefile::new(handle))
            })
    }

    /// Reopen a `Savefile` context for recording captured packets using this `Capture`'s
    /// configurations. This is similar to `savefile()` but does not create the file if it
    /// does  not exist and, if it does already exist, and is a pcap file with the same
    /// byte order as the host opening the file, and has the same time stamp precision,
    /// link-layer header type,  and  snapshot length as p, it will write new packets
    /// at the end of the file.
    #[cfg(libpcap_1_7_2)]
    pub fn savefile_append<P: AsRef<Path>>(&self, path: P) -> Result<Savefile, Error> {
        let name = CString::new(path.as_ref().to_str().unwrap())?;
        let handle = unsafe { raw::pcap_dump_open_append(*self.handle, name.as_ptr()) };
        self.check_err(!handle.is_null()).map(|_| Savefile::new(handle))
    }

    /// Set the direction of the capture
    pub fn direction(&self, direction: Direction) -> Result<(), Error> {
        self.check_err(unsafe { raw::pcap_setdirection(*self.handle, direction as u32 as _) == 0 })
    }

    /// Blocks until a packet is returned from the capture handle or an error occurs.
    ///
    /// pcap captures packets and places them into a buffer which this function reads
    /// from. This buffer has a finite length, so if the buffer fills completely new
    /// packets will be discarded temporarily. This means that in realtime situations,
    /// you probably want to minimize the time between calls of this next() method.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Packet, Error> {
        unsafe {
            let mut header: *mut raw::pcap_pkthdr = ptr::null_mut();
            let mut packet: *const libc::c_uchar = ptr::null();
            let retcode = raw::pcap_next_ex(*self.handle, &mut header, &mut packet);
            self.check_err(retcode != -1)?; // -1 => an error occured while reading the packet
            match retcode {
                i if i >= 1 => {
                    // packet was read without issue
                    Ok(Packet::new(&*(&*header as *const raw::pcap_pkthdr as *const PacketHeader),
                                   slice::from_raw_parts(packet, (*header).caplen as _)))
                }
                0 => {
                    // packets are being read from a live capture and the
                    // timeout expired
                    Err(TimeoutExpired)
                }
                -2 => {
                    // packets are being read from a "savefile" and there are no
                    // more packets to read
                    Err(NoMorePackets)
                }
                _ => {
                    // libpcap only defines codes >=1, 0, -1, and -2
                    unreachable!()
                }
            }
        }
    }

    #[cfg(feature = "capture-stream")]
    fn next_noblock<'a>(&'a mut self, cx: &mut core::task::Context, fd: &mut tokio::io::PollEvented<stream::SelectableFd>) -> Result<Packet<'a>, Error> {
        if let futures::task::Poll::Pending = fd.poll_read_ready(cx, mio::Ready::readable()) {
            Err(IoError(io::ErrorKind::WouldBlock))
        } else {
            match self.next() {
                Ok(p) => Ok(p),
                Err(TimeoutExpired) => {
                    fd.clear_read_ready(cx, mio::Ready::readable())?;
                    Err(IoError(io::ErrorKind::WouldBlock))
                }
                Err(e) => Err(e)
            }
        }
    }

    #[cfg(feature = "capture-stream")]
    pub fn stream<C: stream::PacketCodec>(self, codec: C) -> Result<stream::PacketStream<T, C>, Error> {
        if !self.nonblock {
            return Err(NonNonBlock);
        }
        unsafe {
            let fd = raw::pcap_get_selectable_fd(*self.handle);
            stream::PacketStream::new(self, fd, codec)
        }
    }

    /// Adds a filter to the capture using the given BPF program string. Internally
    /// this is compiled using `pcap_compile()`.
    ///
    /// See http://biot.com/capstats/bpf.html for more information about this syntax.
    pub fn filter(&mut self, program: &str) -> Result<(), Error> {
        let program = CString::new(program)?;
        unsafe {
            let mut bpf_program: raw::bpf_program = mem::zeroed();
            let ret = raw::pcap_compile(*self.handle, &mut bpf_program, program.as_ptr(), 0, 0);
            self.check_err(ret != -1)?;
            let ret = raw::pcap_setfilter(*self.handle, &mut bpf_program);
            raw::pcap_freecode(&mut bpf_program);
            self.check_err(ret != -1)
        }
    }

    pub fn stats(&mut self) -> Result<Stat, Error> {
        unsafe {
            let mut stats: raw::pcap_stat = mem::zeroed();
            self.check_err(raw::pcap_stats(*self.handle, &mut stats) != -1)
                .map(|_| Stat::new(stats.ps_recv, stats.ps_drop, stats.ps_ifdrop))
        }
    }
}

impl Capture<Active> {
    /// Sends a packet over this capture handle's interface.
    pub fn sendpacket<B: Borrow<[u8]>>(&mut self, buf: B) -> Result<(), Error> {
        let buf = buf.borrow();
        self.check_err(unsafe {
            raw::pcap_sendpacket(*self.handle, buf.as_ptr() as _, buf.len() as _) == 0
        })
    }

    pub fn setnonblock(mut self) -> Result<Capture<Active>, Error> {
        with_errbuf(|err| unsafe {
            if raw::pcap_setnonblock(*self.handle, 1, err) != 0 {
                return Err(Error::new(err));
            }
            self.nonblock = true;
            Ok(self)
        })
    }
}

impl Capture<Dead> {
    /// Creates a "fake" capture handle for the given link type.
    pub fn dead(linktype: Linktype) -> Result<Capture<Dead>, Error> {
        unsafe { raw::pcap_open_dead(linktype.0, 65535).as_mut() }
        .map(|h| Capture::new(h))
            .ok_or(InsufficientMemory)
    }
}

#[cfg(not(windows))]
impl AsRawFd for Capture<Active> {
    fn as_raw_fd(&self) -> RawFd {
        unsafe {
            let fd = raw::pcap_fileno(*self.handle);

            match fd {
                -1 => {
                    panic!("Unable to get file descriptor for live capture");
                }
                fd => fd,
            }
        }
    }
}

impl<T: State + ? Sized> Drop for Capture<T> {
    fn drop(&mut self) {
        unsafe { raw::pcap_close(*self.handle) }
    }
}

impl<T: Activated> From<Capture<T>> for Capture<dyn Activated> {
    fn from(cap: Capture<T>) -> Capture<dyn Activated> {
        unsafe { mem::transmute(cap) }
    }
}

/// Abstraction for writing pcap savefiles, which can be read afterwards via `Capture::from_file()`.
pub struct Savefile {
    handle: Unique<raw::pcap_dumper_t>,
}

impl Savefile {
    pub fn write(&mut self, packet: &Packet) {
        unsafe {
            raw::pcap_dump(*self.handle as _,
                           &*(packet.header as *const PacketHeader as *const raw::pcap_pkthdr),
                           packet.data.as_ptr());
        }
    }
}

impl Savefile {
    fn new(handle: *mut raw::pcap_dumper_t) -> Savefile {
        unsafe { Savefile { handle: Unique::new(handle) } }
    }
}

impl Drop for Savefile {
    fn drop(&mut self) {
        unsafe { raw::pcap_dump_close(*self.handle) }
    }
}

#[cfg(not(windows))]
pub fn open_raw_fd(fd: RawFd, mode: u8) -> Result<*mut libc::FILE, Error> {
    let mode = vec![mode, 0];
    unsafe { libc::fdopen(fd, mode.as_ptr() as _).as_mut() }.map(|f| f as _).ok_or(InvalidRawFd)
}

#[inline]
fn cstr_to_string(ptr: *const libc::c_char) -> Result<Option<String>, Error> {
    let string = if ptr.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(ptr as _) }.to_str()?.to_owned())
    };
    Ok(string)
}

#[inline]
fn with_errbuf<T, F>(func: F) -> Result<T, Error>
where F: FnOnce(*mut libc::c_char) -> Result<T, Error>
{
    let mut errbuf = [0i8; 256];
    func(errbuf.as_mut_ptr() as _)
}

#[test]
fn test_struct_size() {
    use std::mem::size_of;
    assert_eq!(size_of::<PacketHeader>(), size_of::<raw::pcap_pkthdr>());
}
