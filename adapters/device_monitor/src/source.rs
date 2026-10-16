//! The Windows side: what is plugged in now, when that changes, and what the batteries say.
//!
//! Design (see the plan, sections 4.1 and 15):
//! * Windows is asked to *tell us* when any device arrives or leaves (`CM_Register_Notification`, two registrations: one for
//!   device interfaces, one for device instances). The callback only sets a flag and wakes the worker. It never allocates
//!   much, never blocks, never panics across the foreign boundary.
//! * An event is a trigger, not data. The worker then lists the devices that are present (a cheap list of instance ids),
//!   reads the properties only of ones it has not seen, groups the nodes by container id (one physical device), and
//!   compares with the last list. That also makes a lost event harmless: the next trigger or the periodic reconcile
//!   finds the difference.
//! * Nodes that belong to the computer itself (the null container id) are never devices.

use super::model::*;
use super::rules::{self, NodeInfo};
use std::collections::{BTreeMap, HashMap};

/// A device as seen in one listing: its description, and the node to read the battery from (if it can have one).
#[derive(Debug, Clone)]
pub struct Group {
    pub info: DeviceInfo,
    pub battery_node: Option<String>,
}

/// Classes that can make up a device the user plugs in or pairs.
const WANTED_CLASSES: &[&str] = &[
    "usb", "hidclass", "keyboard", "mouse", "bluetooth", "media", "audioendpoint", "diskdrive", "camera", "image", "printer",
    "printqueue", "wpd", "xboxcomposite", "usbdevice", "smartcardreader",
];

/// Names that mean "plumbing", not a device the user would call one.
const PLUMBING: &[&str] = &["root hub", "host controller", "generic usb hub", "usb hub", "enumerator", "radio", "wireless bluetooth(r)", "rfcomm", "gatt", "generic access"];

pub fn is_plumbing(n: &NodeInfo) -> bool {
    let l = format!("{} {}", n.name, n.bus_desc).to_lowercase();
    PLUMBING.iter().any(|p| l.contains(p)) && !n.instance_id.to_ascii_uppercase().starts_with("BTHENUM\\DEV_") && !n.instance_id.to_ascii_uppercase().starts_with("BTHLE\\DEV_")
}

/// Group nodes into devices. Pure, so it is tested with real instance ids.
pub fn group_nodes(nodes: &[NodeInfo]) -> Vec<Group> {
    let mut by_container: BTreeMap<String, Vec<NodeInfo>> = BTreeMap::new();
    for n in nodes {
        if rules::is_internal_container(&n.container) {
            continue;
        }
        by_container.entry(n.container.to_ascii_lowercase()).or_default().push(n.clone());
    }
    let mut out = Vec::new();
    for (_, group) in by_container {
        let wanted = group.iter().any(|n| WANTED_CLASSES.contains(&n.class.to_ascii_lowercase().as_str()) && !is_plumbing(n));
        if !wanted {
            continue;
        }
        let info = rules::describe(&group);
        let battery_node = group
            .iter()
            .find(|n| {
                let u = n.instance_id.to_ascii_uppercase();
                u.starts_with("BTHENUM\\DEV_") || u.starts_with("BTHLE\\DEV_")
            })
            .map(|n| n.instance_id.clone());
        out.push(Group { info, battery_node });
    }
    // two groups with the same id (a dual-mode Bluetooth device shows up twice): keep the one with the better type
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut dedup: Vec<Group> = Vec::new();
    for g in out {
        match seen.get(&g.info.id) {
            Some(&i) => {
                if dedup[i].info.device_type == DeviceType::BluetoothOther && g.info.device_type != DeviceType::BluetoothOther {
                    dedup[i] = g;
                }
            }
            None => {
                seen.insert(g.info.id.clone(), dedup.len());
                dedup.push(g);
            }
        }
    }
    dedup
}

#[cfg(windows)]
pub mod win {
    use super::*;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::Arc;
    use windows_sys::core::GUID;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
    use windows_sys::Win32::Devices::Properties::*;
    use windows_sys::Win32::Foundation::DEVPROPKEY;
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    use windows_sys::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN};

    /// The (undocumented) property Windows fills with a Bluetooth device's battery percentage when the device reports one.
    const DEVPKEY_BLUETOOTH_BATTERY: DEVPROPKEY = DEVPROPKEY {
        fmtid: GUID { data1: 0x104EA319, data2: 0x6EE2, data3: 0x4701, data4: [0xBD, 0x47, 0x8D, 0xDB, 0xF4, 0x25, 0xBB, 0xE5] },
        pid: 2,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Present device instance ids.
    pub fn present_instance_ids() -> Vec<String> {
        // SAFETY: both calls get a correctly sized buffer; the list is a sequence of NUL-terminated strings ending with an empty one.
        unsafe {
            let mut len: u32 = 0;
            if CM_Get_Device_ID_List_SizeW(&mut len, std::ptr::null(), CM_GETIDLIST_FILTER_PRESENT) != CR_SUCCESS || len < 2 {
                return Vec::new();
            }
            let mut buf = vec![0u16; len as usize];
            if CM_Get_Device_ID_ListW(std::ptr::null(), buf.as_mut_ptr(), len, CM_GETIDLIST_FILTER_PRESENT) != CR_SUCCESS {
                return Vec::new();
            }
            buf.split(|c| *c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
        }
    }

    fn locate(instance_id: &str) -> Option<u32> {
        let mut dn: u32 = 0;
        // SAFETY: valid NUL-terminated string and out pointer.
        (unsafe { CM_Locate_DevNodeW(&mut dn, wide(instance_id).as_ptr(), CM_LOCATE_DEVNODE_NORMAL) } == CR_SUCCESS).then_some(dn)
    }

    fn prop_bytes(dn: u32, key: &DEVPROPKEY) -> Option<(u32, Vec<u8>)> {
        let mut ty: u32 = 0;
        let mut size: u32 = 0;
        // SAFETY: first call asks for the size; second reads into a buffer of that size.
        unsafe {
            let r = CM_Get_DevNode_PropertyW(dn, key, &mut ty, std::ptr::null_mut(), &mut size, 0);
            if r != CR_BUFFER_SMALL && r != CR_SUCCESS || size == 0 {
                return None;
            }
            let mut buf = vec![0u8; size as usize];
            (CM_Get_DevNode_PropertyW(dn, key, &mut ty, buf.as_mut_ptr(), &mut size, 0) == CR_SUCCESS).then(|| {
                buf.truncate(size as usize);
                (ty, buf)
            })
        }
    }

    fn prop_string(dn: u32, key: &DEVPROPKEY) -> String {
        match prop_bytes(dn, key) {
            Some((t, b)) if t == DEVPROP_TYPE_STRING => {
                let u: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|c| *c != 0).collect();
                String::from_utf16_lossy(&u)
            }
            _ => String::new(),
        }
    }

    fn prop_guid(dn: u32, key: &DEVPROPKEY) -> String {
        match prop_bytes(dn, key) {
            Some((t, b)) if t == DEVPROP_TYPE_GUID && b.len() >= 16 => {
                let d1 = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                let d2 = u16::from_le_bytes([b[4], b[5]]);
                let d3 = u16::from_le_bytes([b[6], b[7]]);
                format!("{{{d1:08x}-{d2:04x}-{d3:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}}}", b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15])
            }
            _ => String::new(),
        }
    }

    fn read_node(instance_id: &str) -> Option<NodeInfo> {
        let dn = locate(instance_id)?;
        let friendly = prop_string(dn, &DEVPKEY_Device_FriendlyName);
        let name = if friendly.is_empty() { prop_string(dn, &DEVPKEY_Device_DeviceDesc) } else { friendly };
        Some(NodeInfo {
            instance_id: instance_id.to_string(),
            class: prop_string(dn, &DEVPKEY_Device_Class),
            name,
            bus_desc: prop_string(dn, &DEVPKEY_Device_BusReportedDeviceDesc),
            manufacturer: prop_string(dn, &DEVPKEY_Device_Manufacturer),
            container: prop_guid(dn, &DEVPKEY_Device_ContainerId),
        })
    }

    /// Reads node properties once per instance id and remembers them (they do not change while the node exists).
    #[derive(Default)]
    pub struct Enumerator {
        cache: HashMap<String, Option<NodeInfo>>,
    }

    const PREFIXES: &[&str] = &["USB\\", "USBSTOR\\", "BTHENUM\\", "BTHLE\\", "HID\\", "SWD\\MMDEVAPI\\", "WPDBUSENUM\\", "STORAGE\\", "SCSI\\", "UMB\\", "XUSB\\"];

    impl Enumerator {
        /// Everything that is plugged in or connected now, grouped into devices.
        pub fn devices(&mut self) -> Vec<Group> {
            let ids: Vec<String> = present_instance_ids().into_iter().filter(|id| PREFIXES.iter().any(|p| id.to_ascii_uppercase().starts_with(p))).collect();
            let live: std::collections::HashSet<&String> = ids.iter().collect();
            self.cache.retain(|k, _| live.contains(k));
            let mut nodes = Vec::with_capacity(ids.len());
            for id in &ids {
                let n = self.cache.entry(id.clone()).or_insert_with(|| read_node(id));
                if let Some(n) = n {
                    nodes.push(n.clone());
                }
            }
            group_nodes(&nodes)
        }
    }

    /// The battery percentage a Bluetooth device node publishes, if it does.
    pub fn bluetooth_battery(instance_id: &str) -> Option<BatteryReading> {
        let dn = locate(instance_id)?;
        let (_, b) = prop_bytes(dn, &DEVPKEY_BLUETOOTH_BATTERY)?;
        let value = match b.len() {
            1 => b[0] as u32,
            2 => u16::from_le_bytes([b[0], b[1]]) as u32,
            4 => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            _ => return None,
        };
        // 255 is a "no value" marker; anything above 100 is not a percentage
        (value <= 100).then_some(BatteryReading { percent: Some(value as u8), charging: Charging::Unknown, source: BatterySource::WindowsProperty })
    }

    /// This computer's own battery, or None on a machine without one.
    pub fn system_battery() -> Option<BatteryReading> {
        // SAFETY: plain data filled by the call.
        let s: SYSTEM_POWER_STATUS = unsafe {
            let mut s: SYSTEM_POWER_STATUS = std::mem::zeroed();
            if GetSystemPowerStatus(&mut s) == 0 {
                return None;
            }
            s
        };
        if s.BatteryFlag == 255 || s.BatteryFlag & 128 != 0 {
            return None; // unknown or no battery
        }
        let percent = (s.BatteryLifePercent <= 100).then_some(s.BatteryLifePercent);
        let on_ac = s.ACLineStatus == 1;
        let charging = if s.BatteryFlag & 8 != 0 {
            Charging::Charging
        } else if on_ac && percent == Some(100) {
            Charging::Full
        } else if on_ac {
            Charging::NotCharging
        } else {
            Charging::Discharging
        };
        Some(BatteryReading { percent, charging, source: BatterySource::System })
    }

    pub fn computer_info() -> DeviceInfo {
        DeviceInfo { id: "system:battery".into(), id_kind: IdKind::System, name: String::new(), device_type: DeviceType::Computer, manufacturer: String::new(), connection: Connection::Internal }
    }

    /// A full-screen app or a presentation is running: only urgent announcements should speak.
    pub fn quiet_reason() -> Option<&'static str> {
        // SAFETY: the out parameter is a plain integer.
        unsafe {
            let mut state = 0;
            if SHQueryUserNotificationState(&mut state) != 0 {
                return None;
            }
            if state == QUNS_BUSY || state == QUNS_RUNNING_D3D_FULL_SCREEN {
                Some("a full-screen app is running")
            } else if state == QUNS_PRESENTATION_MODE {
                Some("a presentation is running")
            } else {
                None
            }
        }
    }

    // ------------------------------------------------------------------------------------------------ notifications

    /// What the callbacks signal: "something changed, look again".
    pub struct Wake {
        pub flag: AtomicBool,
        pub tx: Sender<()>,
    }

    unsafe extern "system" fn on_change(_h: HCMNOTIFICATION, context: *const c_void, _action: CM_NOTIFY_ACTION, _data: *const CM_NOTIFY_EVENT_DATA, _size: u32) -> u32 {
        // Never let a panic cross into Windows; do the least possible work on this thread.
        let _ = std::panic::catch_unwind(|| {
            // SAFETY: `context` is the `Wake` leaked by `Registrations::new`, alive until after unregistering.
            let wake = unsafe { &*(context as *const Wake) };
            if !wake.flag.swap(true, Ordering::SeqCst) {
                let _ = wake.tx.send(());
            }
        });
        0
    }

    /// The two registrations. Dropping unregisters, in the safe order.
    pub struct Registrations {
        interface: HCMNOTIFICATION,
        instance: HCMNOTIFICATION,
        wake: *mut Wake,
        pub started: bool,
    }

    // The raw pointers are only used by the owner and by Windows' callbacks, which only read through `Wake`'s atomics.
    unsafe impl Send for Registrations {}

    impl Registrations {
        pub fn new(tx: Sender<()>) -> (Registrations, Arc<AtomicBool>, Vec<String>) {
            let wake = Box::into_raw(Box::new(Wake { flag: AtomicBool::new(false), tx }));
            let mut errors = Vec::new();
            let mut r = Registrations { interface: std::ptr::null_mut(), instance: std::ptr::null_mut(), wake, started: false };
            // SAFETY: filters are fully initialised; the callback and context outlive the registrations (freed in `drop` after unregistering).
            unsafe {
                let mut f1: CM_NOTIFY_FILTER = std::mem::zeroed();
                f1.cbSize = std::mem::size_of::<CM_NOTIFY_FILTER>() as u32;
                f1.Flags = CM_NOTIFY_FILTER_FLAG_ALL_INTERFACE_CLASSES;
                f1.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE;
                let rc = CM_Register_Notification(&f1, wake as *const c_void, Some(on_change), &mut r.interface);
                if rc != CR_SUCCESS {
                    errors.push(format!("device interface notification failed (code {rc})"));
                    r.interface = std::ptr::null_mut();
                }
                let mut f2: CM_NOTIFY_FILTER = std::mem::zeroed();
                f2.cbSize = std::mem::size_of::<CM_NOTIFY_FILTER>() as u32;
                f2.Flags = CM_NOTIFY_FILTER_FLAG_ALL_DEVICE_INSTANCES;
                f2.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEINSTANCE;
                let rc = CM_Register_Notification(&f2, wake as *const c_void, Some(on_change), &mut r.instance);
                if rc != CR_SUCCESS {
                    errors.push(format!("device instance notification failed (code {rc})"));
                    r.instance = std::ptr::null_mut();
                }
            }
            r.started = !r.interface.is_null() || !r.instance.is_null();
            // a flag the worker clears after it has looked
            let flag = Arc::new(AtomicBool::new(false));
            (r, flag, errors)
        }

        /// The worker calls this after it has handled a wake-up, so the next change can wake it again.
        pub fn rearm(&self) {
            // SAFETY: `wake` is valid until drop.
            unsafe { (*self.wake).flag.store(false, Ordering::SeqCst) };
        }
    }

    impl Drop for Registrations {
        fn drop(&mut self) {
            // Fixed order: unregister (this waits for a running callback to finish, so it must not be called from one), then free the context.
            // SAFETY: handles came from CM_Register_Notification; the context is freed once, after both are unregistered.
            unsafe {
                if !self.interface.is_null() {
                    CM_Unregister_Notification(self.interface);
                }
                if !self.instance.is_null() {
                    CM_Unregister_Notification(self.instance);
                }
                drop(Box::from_raw(self.wake));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(id: &str, class: &str, name: &str, bus: &str, container: &str) -> NodeInfo {
        NodeInfo { instance_id: id.into(), class: class.into(), name: name.into(), bus_desc: bus.into(), manufacturer: String::new(), container: container.into() }
    }
    const MACHINE: &str = "{00000000-0000-0000-ffff-ffffffffffff}";

    #[test]
    fn the_computers_own_parts_are_not_devices() {
        let nodes = vec![
            n("USB\\ROOT_HUB30\\4&1A2B&0&0", "USB", "USB Root Hub (USB 3.0)", "", MACHINE),
            n("USB\\VID_8087&PID_0026\\5&5DD469C&0&10", "Bluetooth", "Intel(R) Wireless Bluetooth(R)", "", MACHINE),
            n("SWD\\MMDEVAPI\\{0.0.0.00000000}.{abc}", "AudioEndpoint", "Speakers (Realtek(R) Audio)", "", MACHINE),
            n("USB\\VID_13D3&PID_56FF\\5&1&0&6", "Camera", "Integrated Camera", "", MACHINE),
        ];
        assert!(group_nodes(&nodes).is_empty());
    }

    #[test]
    fn the_five_nodes_of_one_bluetooth_headset_make_one_device() {
        let c = "{11111111-2222-3333-4444-555555555555}";
        let nodes = vec![
            n("BTHENUM\\DEV_D09FF9618AA1\\7&39A909B9&3&BLUETOOTHDEVICE_D09FF9618AA1", "Bluetooth", "Rockerz 551ANC", "", c),
            n("BTHENUM\\{0000110E-0000-1000-8000-00805F9B34FB}_LOCALMFG&0002\\7&39A909B9&3&D09FF9618AA1_C00000000", "Bluetooth", "Rockerz 551ANC Avrcp Transport", "", c),
            n("BTHENUM\\{0000110C-0000-1000-8000-00805F9B34FB}_LOCALMFG&0002\\7&39A909B9&3&D09FF9618AA1_C00000000", "Bluetooth", "Rockerz 551ANC Avrcp Transport", "", c),
            n("SWD\\MMDEVAPI\\{0.0.0.00000000}.{aaaa}", "AudioEndpoint", "Headphones (Rockerz 551ANC Stereo)", "", c),
            n("SWD\\MMDEVAPI\\{0.0.1.00000000}.{bbbb}", "AudioEndpoint", "Headset (Rockerz 551ANC Hands-Free)", "", c),
        ];
        let g = group_nodes(&nodes);
        assert_eq!(g.len(), 1);
        assert_eq!((g[0].info.id.as_str(), g[0].info.device_type, g[0].info.name.as_str()), ("bt:d09ff9618aa1", DeviceType::Headphones, "Rockerz 551ANC"));
        assert!(g[0].battery_node.as_deref().unwrap().starts_with("BTHENUM\\DEV_D09F"));
    }

    #[test]
    fn a_usb_stick_and_a_usb_mouse_are_two_devices_with_their_own_kinds() {
        let (a, b) = ("{aaaaaaaa-1111-1111-1111-111111111111}", "{bbbbbbbb-2222-2222-2222-222222222222}");
        let nodes = vec![
            n("USB\\VID_0781&PID_5581\\4C530001230123116465", "USB", "USB Mass Storage Device", "SanDisk Ultra", a),
            n("USBSTOR\\DISK&VEN_SANDISK&PROD_ULTRA&REV_1.00\\4C530001230123116465&0", "DiskDrive", "SanDisk Ultra USB Device", "", a),
            n("USB\\VID_046D&PID_C52B\\5&2D1A3C4B&0&2", "USB", "USB Composite Device", "", b),
            n("HID\\VID_046D&PID_C52B&MI_01\\7&1B2C3D4E&0&0000", "Mouse", "HID-compliant mouse", "", b),
        ];
        let g = group_nodes(&nodes);
        assert_eq!(g.len(), 2);
        let kinds: Vec<DeviceType> = g.iter().map(|x| x.info.device_type).collect();
        assert!(kinds.contains(&DeviceType::UsbStorage) && kinds.contains(&DeviceType::Mouse), "{kinds:?}");
        assert!(g.iter().all(|x| x.battery_node.is_none()));
    }

    #[test]
    fn a_dual_mode_bluetooth_device_is_not_listed_twice() {
        let (a, b) = ("{aaaaaaaa-1111-1111-1111-111111111111}", "{bbbbbbbb-2222-2222-2222-222222222222}");
        let nodes = vec![
            n("BTHENUM\\DEV_E44097697AF6\\7&39A909B9&3&BLUETOOTHDEVICE_E44097697AF6", "Bluetooth", "OPPO Reno10 5G", "", a),
            n("BTHENUM\\{0000112F-0000-1000-8000-00805F9B34FB}_VID&00010046_PID&1200\\7&39A909B9&3&E44097697AF6_C00000000", "Bluetooth", "OPPO Reno10 5G Phonebook", "", a),
            n("BTHLE\\DEV_E44097697AF6\\7&14C3891A&3&E44097697AF6", "Bluetooth", "OPPO Reno10 5G", "", b),
        ];
        let g = group_nodes(&nodes);
        assert_eq!(g.len(), 1, "{g:?}");
        assert_eq!(g[0].info.device_type, DeviceType::Phone);
    }

    #[test]
    fn plumbing_nodes_alone_are_not_devices() {
        let c = "{cccccccc-3333-3333-3333-333333333333}";
        assert!(group_nodes(&[n("BTH\\MS_BTHBRB\\6&1303089C&0&1", "Bluetooth", "Microsoft Bluetooth Enumerator", "", c)]).is_empty());
        assert!(group_nodes(&[n("USB\\VID_1234&PID_0001\\5&1&0&1", "USB", "Generic USB Hub", "", c)]).is_empty());
    }
}

#[cfg(all(test, windows))]
mod live {
    use super::win::*;

    /// Runs against this very machine: it must not panic, must find no internal part as a device, and must give each device one id.
    #[test]
    fn this_machine_can_be_enumerated() {
        let mut e = Enumerator::default();
        let t = std::time::Instant::now();
        let devices = e.devices();
        let first = t.elapsed();
        let t = std::time::Instant::now();
        let again = e.devices();
        let second = t.elapsed();
        eprintln!("enumerated {} device(s) in {first:?}, again in {second:?} (properties cached)", devices.len());
        for d in &devices {
            eprintln!("  {:?} {:<28} {:?} {:?} id={} battery_node={}", d.info.device_type, d.info.name, d.info.connection, d.info.id_kind, d.info.id, d.battery_node.is_some());
        }
        assert_eq!(devices.len(), again.len());
        let mut ids: Vec<&String> = devices.iter().map(|d| &d.info.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), devices.len(), "one id per device");
        assert!(second < std::time::Duration::from_secs(2), "a second listing is cheap, took {second:?}");
    }

    #[test]
    fn the_computers_battery_can_be_read_or_is_absent() {
        match system_battery() {
            Some(b) => eprintln!("system battery: {b:?}"),
            None => eprintln!("no battery on this machine"),
        }
    }

    #[test]
    fn notifications_register_and_unregister_cleanly_while_being_flooded() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (reg, _flag, errors) = Registrations::new(tx);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(reg.started);
        // let Windows deliver whatever it has for a moment, then unregister while that may still be happening
        std::thread::sleep(std::time::Duration::from_millis(300));
        while rx.try_recv().is_ok() {}
        drop(reg);
    }
}
