// SPDX-FileCopyrightText: 2021 Kent Gibson <warthog618@gmail.com>
//
// SPDX-License-Identifier: MIT

use bitflags::bitflags;
use libc::ioctl;
use std::fs::File;
use std::io::Error as IoError;
use std::mem::size_of;
use std::os::unix::io::{AsRawFd, FromRawFd};

use super::common::{ValidationResult, IOCTL_MAGIC};

// common to ABI v1 and v2.
pub use super::common::{
    get_chip_info, unwatch_line_info, ChipInfo, InfoChangeKind, LineEdgeEventKind, Offset, Offsets,
    Padding, ValidationError,
};
use super::{Error, Name, Result};

#[repr(u8)]
enum Ioctl {
    GetLineInfo = 2,
    GetLineHandle = 3,
    GetLineEvent = 4,
    GetLineValues = 8,
    SetLineValues = 9,
    SetConfig = 0xA,
    WatchLineInfo = 0xB,
}

/// Information about a certain GPIO line.
#[repr(C)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LineInfo {
    /// The line offset on this GPIO device.
    /// This is the identifier used when requesting the line from the kernel.
    pub offset: Offset,
    /// The configuration flags for this line.
    pub flags: LineInfoFlags,
    /// The name of this GPIO line, such as the output pin of the line on the
    /// chip, a rail or a pin header name on a board, as specified by the GPIO
    /// chip.
    ///
    /// May be empty.
    pub name: Name,
    /// A functional name for the consumer of this GPIO line as set by
    /// whatever is using it.
    ///
    /// Will be empty if there is no current user but may
    /// also be empty if the consumer doesn't set a consumer name.
    pub consumer: Name,
}

bitflags! {
    /// Flags indicating the configuration of a line.
    #[derive(Default)]
    pub struct LineInfoFlags: u32 {
        /// The line is in use and is not available for request.
        const USED = 1;
        /// The line is an output.
        const OUTPUT = 2;
        /// The line active state corresponds to a physical low.
        const ACTIVE_LOW = 4;
        /// The line is an open drain output.
        const OPEN_DRAIN = 8;
        /// The line is an open source output.
        const OPEN_SOURCE = 16;
        /// The line has pull-up bias enabled.
        const BIAS_PULL_UP = 32;
        /// The line has pull-down bias enabled.
        const BIAS_PULL_DOWN = 64;
        /// The line has bias disabled.
        const BIAS_DISABLED = 128;
    }
}

/// Get the publicly available information for a line.
///
/// This does not include the line value.
/// The line must be requested to access the value.
///
/// * `cf` - The open chip File.
/// * `offset` - The offset of the line.
pub fn get_line_info(cf: &File, offset: Offset) -> Result<LineInfo> {
    let li = LineInfo {
        offset,
        ..Default::default()
    };
    match unsafe {
        ioctl(
            cf.as_raw_fd(),
            nix::request_code_readwrite!(IOCTL_MAGIC, Ioctl::GetLineInfo, size_of::<LineInfo>()),
            &li,
        )
    } {
        0 => Ok(li),
        _ => Err(Error::from(IoError::last_os_error())),
    }
}

/// Add a watch on changes to the [`LineInfo`] for a line.
///
/// Returns the current state of that information.
/// This does not include the line value.
/// The line must be requested to access the value.
///
/// * `cf` - The open chip File.
/// * `offset` - The offset of the line to watch.
pub fn watch_line_info(cf: &File, offset: Offset) -> Result<LineInfo> {
    let li = LineInfo {
        offset,
        ..Default::default()
    };
    match unsafe {
        ioctl(
            cf.as_raw_fd(),
            nix::request_code_readwrite!(IOCTL_MAGIC, Ioctl::WatchLineInfo, size_of::<LineInfo>()),
            &li,
        )
    } {
        0 => Ok(li),
        _ => Err(Error::from(IoError::last_os_error())),
    }
}

/// Information about a change in status of a GPIO line.
#[repr(C)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineInfoChangeEvent {
    /// Updated line information.
    pub info: LineInfo,
    /// An estimate of time of status change occurrence, in nanoseconds.
    pub timestamp_ns: u64,
    /// The kind of change event.
    pub kind: InfoChangeKind,
    /// Reserved for future use.
    #[doc(hidden)]
    pub padding: Padding<5>,
}

impl LineInfoChangeEvent {
    /// Read a LineInfoChangeEvent from a buffer.
    ///
    /// The buffer is assumed to have been populated by a read of the chip File,
    /// so the content is validated before being returned.
    pub fn from_buf(d: &[u8]) -> Result<&LineInfoChangeEvent> {
        let le = unsafe { &*(d as *const [u8] as *const LineInfoChangeEvent) };
        le.validate().map(|_| le).map_err(Error::from)
    }

    /// Check that a LineInfoChangeEvent read from the kernel is valid in Rust.
    fn validate(&self) -> ValidationResult {
        self.kind
            .validate()
            .map_err(|e| ValidationError::new("kind", &e))
    }
}

/// Information about a GPIO line handle request.
#[repr(C)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HandleRequest {
    /// An array of requested lines, identitifed by offset on the associated GPIO device.
    pub offsets: Offsets,
    /// The requested flags for the requested GPIO lines.
    ///
    /// Note that even if multiple lines are requested, the same flags must be applicable
    /// to all of them, if you want lines with individual flags set, request them one by one.
    /// It is possible to select a batch of input or output lines, but they must all
    /// have the same characteristics, i.e. all inputs or all outputs, all active low etc.
    pub flags: HandleRequestFlags,
    /// If the [`HandleRequestFlags::OUTPUT`] is set for a requested line, this specifies the
    /// output value for each offset.  Should be 0 (*inactive*) or 1 (*active*).
    /// Anything other than 0 or 1 is interpreted as 1 (*active*).
    pub values: LineValues,
    /// A requested consumer label for the selected GPIO line(s) such as "*my-bitbanged-relay*".
    pub consumer: Name,
    /// The number of lines requested in this request, i.e. the number of valid fields in
    /// the `offsets` and `values` arrays.
    ///
    /// Set to 1 to request a single line.
    pub lines: u32,
    /// This field is only present for the underlying ioctl call and is only used internally.
    //
    // This is actually specified as an int in gpio.h, but that presents problems
    // as it is not fixed width.  It is usually i32, so that is what we go with here,
    // but that may cause issues on platforms.
    #[doc(hidden)]
    pub fd: i32,
}

bitflags! {
    /// Configuration flags for requested lines.
    ///
    /// Note that several of the flags, such as BIAS_PULL_UP and BIAS_PULL_DOWN are mutually
    /// exclusive.  The kernel will reject requests with flag combinations that do not make
    /// sense.
    #[derive(Default)]
    pub struct HandleRequestFlags: u32 {
        /// Requests line as an input.
        const INPUT = 1;
        /// Requests line as an output.
        const OUTPUT = 2;
        /// Requests line as active low.
        const ACTIVE_LOW = 4;
        /// Requests line as open drain.
        const OPEN_DRAIN = 8;
        /// Requests line as open source.
        const OPEN_SOURCE = 16;
        /// Requests line with pull-up bias.
        const BIAS_PULL_UP = 32;
        /// Requests line with pull-down bias.
        const BIAS_PULL_DOWN = 64;
        /// Requests line with bias disabled.
        const BIAS_DISABLED = 128;
    }
}

/// Request a line or set of lines for exclusive access.
///
/// * `cf` - The open chip File.
/// * `hr` - The line handle request.
pub fn get_line_handle(cf: &File, hr: HandleRequest) -> Result<File> {
    unsafe {
        match ioctl(
            cf.as_raw_fd(),
            nix::request_code_readwrite!(
                IOCTL_MAGIC,
                Ioctl::GetLineHandle,
                size_of::<HandleRequest>()
            ),
            &hr,
        ) {
            0 => Ok(File::from_raw_fd(hr.fd)),
            _ => Err(Error::from(IoError::last_os_error())),
        }
    }
}

/// Updated configuration for an existing GPIO handle request.
#[repr(C)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HandleConfig {
    /// Updated flags for the requested GPIO lines.
    ///
    /// The flags will be applied to all lines in the existing request.
    pub flags: HandleRequestFlags,
    /// If the [`HandleRequestFlags::OUTPUT`] is set in flags, this specifies the
    /// output value, should be 0 (*inactive*) or 1 (*active*).
    ///
    /// All other values are interpreted as active.
    pub values: LineValues,
    /// Reserved for future use and should be zero filled.
    #[doc(hidden)]
    pub padding: Padding<4>,
}

/// Update the configuration of an existing handle or event request.
///
/// * `lf` - The file returned by [`get_line_handle`].
/// * `hc` - The configuration to be applied.
pub fn set_line_config(cf: &File, hc: HandleConfig) -> Result<()> {
    unsafe {
        match ioctl(
            cf.as_raw_fd(),
            nix::request_code_readwrite!(IOCTL_MAGIC, Ioctl::SetConfig, size_of::<HandleConfig>()),
            &hc,
        ) {
            0 => Ok(()),
            _ => Err(Error::from(IoError::last_os_error())),
        }
    }
}

/// The logical values of the requested lines.
///
/// Values are stored as u8, as that is what the uAPI specifies.
///
/// 0 is *inactive* with 1 and all other values taken as *active*.
///
/// Values are stored in the same order as the offsets in the [`HandleRequest.offsets`].
///
/// Values for input lines are ignored.
///
/// [`HandleRequest.offsets`]: struct@HandleRequest
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineValues([u8; 64usize]);

impl LineValues {
    /// Create values from a slice.
    ///
    /// The values are in the same order as [`HandleRequest.offsets`].
    ///
    /// [`HandleRequest.offsets`]: struct@HandleRequest
    pub fn from_slice(s: &[u8]) -> Self {
        let mut n: LineValues = Default::default();
        for (src, dst) in s.iter().zip(n.0.iter_mut()) {
            *dst = *src;
        }
        n
    }
    /// Return the value of a line.
    ///
    /// Note that the [`LineValues`] need to be populated via a call to [`get_line_values`]
    /// to get values from the underlying hardware.
    ///
    /// * `idx` - The index into the [`HandleRequest.offsets`] for the line of interest.
    ///
    /// [`HandleRequest.offsets`]: struct@HandleRequest
    #[inline]
    pub fn get(&self, idx: usize) -> u8 {
        self.0[idx]
    }
    /// Set the value of a line.
    ///
    /// Note that this is not applied to hardware until these values are passed to
    /// [`set_line_values`].
    ///
    /// * `idx` - The index into the [`HandleRequest.offsets`] for the line of interest.
    /// * `value` - The logical state of the line to be set.
    ///
    /// [`HandleRequest.offsets`]: struct@HandleRequest
    #[inline]
    pub fn set(&mut self, idx: usize, value: u8) {
        self.0[idx] = value;
    }
}
impl Default for LineValues {
    fn default() -> Self {
        LineValues([0; 64])
    }
}

/// Read the values of requested lines.
///
/// * `lf` - The file returned by [`get_line_handle`] or [`get_line_event`].
/// * `vals` - The line values to be populated.
pub fn get_line_values(lf: &File, vals: &mut LineValues) -> Result<()> {
    match unsafe {
        ioctl(
            lf.as_raw_fd(),
            nix::request_code_readwrite!(
                IOCTL_MAGIC,
                Ioctl::GetLineValues,
                size_of::<LineValues>()
            ),
            vals.0.as_mut_ptr(),
        )
    } {
        0 => Ok(()),
        _ => Err(Error::from(IoError::last_os_error())),
    }
}

/// Set the values of requested lines.
///
/// * `lf` - The file returned by [`get_line_handle`].
/// * `vals` - The line values to be set.
pub fn set_line_values(lf: &File, vals: &LineValues) -> Result<()> {
    match unsafe {
        ioctl(
            lf.as_raw_fd(),
            nix::request_code_readwrite!(
                IOCTL_MAGIC,
                Ioctl::SetLineValues,
                size_of::<LineValues>()
            ),
            vals.0.as_ptr(),
        )
    } {
        0 => Ok(()),
        _ => Err(Error::from(IoError::last_os_error())),
    }
}

/// Information about a GPIO event request.
#[repr(C)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EventRequest {
    /// The line to request edge events from, identified by its offset
    /// on the associated GPIO device.
    pub offset: Offset,
    /// The requested handle flags for the GPIO line.
    pub handleflags: HandleRequestFlags,
    /// The requested event flags for the GPIO line.
    pub eventflags: EventRequestFlags,
    /// A requested consumer label for the selected GPIO line(s) such as "*my-listener*".
    pub consumer: Name,
    /// This field is only present for the underlying ioctl call and is only used internally.
    ///
    // This is actually specified as an int in gpio.h, but that presents problems
    // as it is not fixed width.  It is usually i32, so that is what we go with here,
    // though this may cause issues on platforms with a differently sized int.
    #[doc(hidden)]
    pub fd: i32,
}

bitflags! {
    /// Additional configuration flags for event requests.
    #[derive(Default)]
    pub struct EventRequestFlags: u32 {
        /// Report rising edge events on the requested line.
        const RISING_EDGE = 1;
        /// Report falling edge events on the requested line.
        const FALLING_EDGE = 2;
        /// Report both rising and falling edge events on the requested line.
        const BOTH_EDGES = Self::RISING_EDGE.bits | Self::FALLING_EDGE.bits;
    }
}

/// Request a line with edge detection enabled.
///
/// Detected events can be read from the returned file.
///
/// * `cf` - The open chip File.
/// * `er` - The line event request.
pub fn get_line_event(cf: &File, er: EventRequest) -> Result<File> {
    unsafe {
        match ioctl(
            cf.as_raw_fd(),
            nix::request_code_readwrite!(
                IOCTL_MAGIC,
                Ioctl::GetLineEvent,
                size_of::<EventRequest>()
            ),
            &er,
        ) {
            0 => Ok(File::from_raw_fd(er.fd)),
            _ => Err(Error::from(IoError::last_os_error())),
        }
    }
}

/// Information about an edge event on a requested line.
#[repr(C)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineEdgeEvent {
    /// The best estimate of time of event occurrence, in nanoseconds.
    pub timestamp_ns: u64,
    /// The kind of line event.
    pub kind: LineEdgeEventKind,
}

impl LineEdgeEvent {
    /// Read a LineEdgeEvent from a buffer.
    ///
    /// The buffer is assumed to have been populated by a read of the line request File,
    /// so the content is validated before being returned.
    pub fn from_buf(d: &[u8]) -> Result<&LineEdgeEvent> {
        let le = unsafe { &*(d as *const [u8] as *const LineEdgeEvent) };
        le.validate().map(|_| le).map_err(Error::from)
    }

    /// Check that a LineEdgeEvent read from the kernel is valid in Rust.
    fn validate(&self) -> ValidationResult {
        self.kind
            .validate()
            .map_err(|e| ValidationError::new("kind", &e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_size_line_info() {
        assert_eq!(
            size_of::<LineInfo>(),
            72usize,
            concat!("Size of: ", stringify!(LineInfo))
        );
    }
    #[test]
    fn test_size_line_info_changed() {
        assert_eq!(
            size_of::<LineInfoChangeEvent>(),
            104usize,
            concat!("Size of: ", stringify!(LineInfoChangeEvent))
        );
    }
    #[test]
    fn test_size_handle_request() {
        assert_eq!(
            size_of::<HandleRequest>(),
            364usize,
            concat!("Size of: ", stringify!(HandleRequest))
        );
    }
    #[test]
    fn test_size_handle_config() {
        assert_eq!(
            size_of::<HandleConfig>(),
            84usize,
            concat!("Size of: ", stringify!(HandleConfig))
        );
    }
    #[test]
    fn test_size_values() {
        assert_eq!(
            size_of::<LineValues>(),
            64usize,
            concat!("Size of: ", stringify!(LineValues))
        );
    }
    #[test]
    fn test_size_event_request() {
        assert_eq!(
            size_of::<EventRequest>(),
            48usize,
            concat!("Size of: ", stringify!(EventRequest))
        );
    }
    #[test]
    fn test_size_line_event() {
        assert_eq!(
            size_of::<LineEdgeEvent>(),
            16usize,
            concat!("Size of: ", stringify!(LineEdgeEvent))
        );
    }
    #[test]
    fn test_line_info_changed_validate() {
        let mut a = LineInfoChangeEvent {
            info: Default::default(),
            timestamp_ns: 0,
            kind: InfoChangeKind::Released,
            padding: Default::default(),
        };
        assert!(a.validate().is_ok());
        a.timestamp_ns = 1234;
        assert!(a.validate().is_ok());
        unsafe {
            a.kind = *(&0 as *const i32 as *const InfoChangeKind);
            let e = a.validate().unwrap_err();
            assert_eq!(e.field, "kind");
            assert_eq!(e.msg, "invalid value: 0");
            a.kind = *(&4 as *const i32 as *const InfoChangeKind);
            let e = a.validate().unwrap_err();
            assert_eq!(e.field, "kind");
            assert_eq!(e.msg, "invalid value: 4");
            a.kind = *(&1 as *const i32 as *const InfoChangeKind);
            assert!(a.validate().is_ok());
        }
    }
    #[test]
    fn test_line_event_validate() {
        let mut a = LineEdgeEvent {
            timestamp_ns: 0,
            kind: LineEdgeEventKind::RisingEdge,
        };
        assert!(a.validate().is_ok());
        a.timestamp_ns = 1234;
        assert!(a.validate().is_ok());
        unsafe {
            a.kind = *(&0 as *const i32 as *const LineEdgeEventKind);
            let e = a.validate().unwrap_err();
            assert_eq!(e.field, "kind");
            assert_eq!(e.msg, "invalid value: 0");
            a.kind = *(&3 as *const i32 as *const LineEdgeEventKind);
            let e = a.validate().unwrap_err();
            assert_eq!(e.field, "kind");
            assert_eq!(e.msg, "invalid value: 3");
            a.kind = *(&1 as *const i32 as *const LineEdgeEventKind);
            assert!(a.validate().is_ok());
        }
    }
    #[test]
    fn test_line_values_get() {
        let mut a = LineValues::default();
        for idx in [0, 2] {
            assert_eq!(a.0[idx as usize], 0, "idx: {}", idx);
            assert_eq!(a.get(idx), 0, "idx: {}", idx);
            a.0[idx as usize] = 1;
            assert_eq!(a.get(idx), 1, "idx: {}", idx);
            a.0[idx as usize] = 42;
            assert_eq!(a.get(idx), 42, "idx: {}", idx);
        }
    }
    #[test]
    fn test_line_values_set() {
        let mut a = LineValues::default();
        for idx in [0, 2] {
            a.set(idx, 0);
            assert_eq!(a.0[idx as usize], 0, "idx: {}", idx);
            a.set(idx, 1);
            assert_eq!(a.0[idx as usize], 1, "idx: {}", idx);
            a.set(idx, 42);
            assert_eq!(a.0[idx as usize], 42, "idx: {}", idx);
        }
    }
}
