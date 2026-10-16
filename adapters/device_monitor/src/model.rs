//! The data the monitor works with: what a device is, what its battery says, and the events that describe changes.
//! Plain data only, so every other part can be tested without Windows.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceType {
    Keyboard,
    Mouse,
    Headphones,
    Speaker,
    Microphone,
    Controller,
    Camera,
    Printer,
    UsbStorage,
    Phone,
    BluetoothOther,
    UsbUnknown,
    Computer,
    Unknown,
}

impl DeviceType {
    /// How the type is said aloud when the device has no usable name.
    pub fn spoken(self) -> &'static str {
        match self {
            Self::Keyboard => "keyboard",
            Self::Mouse => "mouse",
            Self::Headphones => "headphones",
            Self::Speaker => "speaker",
            Self::Microphone => "microphone",
            Self::Controller => "game controller",
            Self::Camera => "camera",
            Self::Printer => "printer",
            Self::UsbStorage => "USB storage device",
            Self::Phone => "phone",
            Self::BluetoothOther => "Bluetooth device",
            Self::UsbUnknown => "unknown USB device",
            Self::Computer => "computer",
            Self::Unknown => "unknown device",
        }
    }

    pub fn category(self) -> Category {
        match self {
            Self::Keyboard => Category::Keyboards,
            Self::Mouse => Category::Mice,
            Self::Headphones => Category::Headphones,
            Self::Speaker => Category::Speakers,
            Self::Microphone => Category::Microphones,
            Self::Controller => Category::Controllers,
            Self::Camera => Category::Cameras,
            Self::Printer => Category::Printers,
            Self::UsbStorage => Category::Storage,
            Self::Phone => Category::Phones,
            Self::Computer => Category::Computer,
            Self::BluetoothOther | Self::UsbUnknown | Self::Unknown => Category::Other,
        }
    }
}

/// The groups a user can switch on or off as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Keyboards,
    Mice,
    Headphones,
    Speakers,
    Microphones,
    Controllers,
    Cameras,
    Printers,
    Storage,
    Phones,
    Computer,
    Other,
}

impl Category {
    pub const ALL: [Category; 12] = [
        Category::Keyboards,
        Category::Mice,
        Category::Headphones,
        Category::Speakers,
        Category::Microphones,
        Category::Controllers,
        Category::Cameras,
        Category::Printers,
        Category::Storage,
        Category::Phones,
        Category::Computer,
        Category::Other,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Connection {
    Usb,
    Bluetooth,
    Wired,
    Internal,
    Unknown,
}

/// How trustworthy the device's id is. A weak id can change when the device moves to another port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdKind {
    /// Bluetooth address.
    Address,
    /// USB vendor id, product id and serial number.
    UsbSerial,
    /// The Windows container id (one per physical device, but derived from the port for devices without a serial).
    Container,
    /// Built from the name and class only.
    Weak,
    /// This computer itself.
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Stable id. Never the display name.
    pub id: String,
    pub id_kind: IdKind,
    /// The name the device reports, possibly empty or generic. Cleaned before it is spoken (`rules::display_name`).
    pub name: String,
    pub device_type: DeviceType,
    pub manufacturer: String,
    pub connection: Connection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Charging {
    #[default]
    Unknown,
    Charging,
    NotCharging,
    Full,
    Discharging,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatterySource {
    #[default]
    None,
    /// The (undocumented) Windows property Bluetooth devices publish when they report a battery.
    WindowsProperty,
    /// The standard Bluetooth battery service.
    Gatt,
    /// A game controller's coarse battery levels.
    XInputLevels,
    /// The computer's own battery.
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BatteryReading {
    /// 0 to 100, or None when the device does not say.
    pub percent: Option<u8>,
    pub charging: Charging,
    pub source: BatterySource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "reason")]
pub enum SourceHealth {
    Ok,
    Unavailable(String),
    Failed(String),
}

/// What a source reports. The engine turns these into decisions.
#[derive(Debug, Clone, PartialEq)]
pub enum RawEvent {
    /// The device is there (new, or its details changed).
    Present { info: DeviceInfo, battery: Option<BatteryReading> },
    Gone { id: String },
    Battery { id: String, reading: BatteryReading },
    Source { name: String, health: SourceHealth },
}

/// Event names as the request lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventKind {
    DeviceConnected,
    DeviceDisconnected,
    BatteryChanged,
    BatteryLow,
    BatteryVeryLow,
    BatteryCritical,
    BatteryRecovered,
    DeviceCharging,
    DeviceFullyCharged,
    SourceFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Medium,
    High,
}
