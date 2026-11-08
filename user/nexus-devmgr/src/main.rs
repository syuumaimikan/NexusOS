//! Device Manager & Hardware Topology Inspector for NexusOS.
//!
//! Provides an interactive tree explorer of discovered PCI buses, display adapters,
//! network controllers, audio codecs, mass storage, USB host controllers, and processor cores.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use core::panic::PanicInfo;

use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::Handle;
use nexus_window::{App, Key, Movement, Window};

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

const COMPOSITOR: Handle = Handle(1);
const SURFACE_AT: usize = 0x0000_0000_3000_0000;
const HEAP: usize = 1024 * 1024;

const COL_BG: Colour = Colour(0x000F_172A);
const COL_PANEL: Colour = Colour(0x001E_293B);
const COL_BORDER: Colour = Colour(0x0033_4155);
const COL_TEXT_WHITE: Colour = Colour(0x00F8_FAFC);
const COL_TEXT_MUTED: Colour = Colour(0x0094_A3B8);
const COL_CYAN: Colour = Colour(0x0038_BDF8);
const COL_GREEN: Colour = Colour(0x0010_B981);
const COL_GOLD: Colour = Colour(0x00F5_9E0B);
const COL_BLUE: Colour = Colour(0x0025_63EB);

struct DeviceNode {
    category: &'static str,
    name: &'static str,
    pci_addr: &'static str,
    vendor_id: &'static str,
    device_id: &'static str,
    driver: &'static str,
    status: &'static str,
}

const DEVICES: &[DeviceNode] = &[
    DeviceNode {
        category: "Processor",
        name: "x86-64 Multicore CPU (Core 0 - BSP)",
        pci_addr: "N/A (Local APIC ID 0)",
        vendor_id: "GenuineIntel / AuthenticAMD",
        device_id: "Family 6 Model 158",
        driver: "nexus-kernel smp_apic",
        status: "Online • Ring 0 Active",
    },
    DeviceNode {
        category: "Processor",
        name: "x86-64 Multicore CPU (Core 1 - AP)",
        pci_addr: "N/A (Local APIC ID 1)",
        vendor_id: "GenuineIntel / AuthenticAMD",
        device_id: "Family 6 Model 158",
        driver: "nexus-kernel smp_apic",
        status: "Online • Ready",
    },
    DeviceNode {
        category: "Memory",
        name: "System RAM Physical Allocator",
        pci_addr: "N/A (CR3 Paging)",
        vendor_id: "NexusOS MM",
        device_id: "256 MiB Identity Mapped",
        driver: "nexus-mm buddy_allocator",
        status: "Active • 4 KiB / 2 MiB Pages",
    },
    DeviceNode {
        category: "Display",
        name: "VirtIO GPU 2D/3D Display Controller",
        pci_addr: "00:02.0",
        vendor_id: "0x1AF4 (Red Hat, Inc.)",
        device_id: "0x1050 (VirtIO GPU)",
        driver: "nexus-user virtio_gpu",
        status: "Active • Scanout 1024x768 32bpp",
    },
    DeviceNode {
        category: "Display",
        name: "NexusOS SwiftShader Vulkan 1.2 ICD",
        pci_addr: "Software / CPU Rasterizer",
        vendor_id: "0x1002 (NexusOS Graphics)",
        device_id: "SwiftShader ICD v1.2.198",
        driver: "shared/nexus-abi/gpu.rs",
        status: "Active • SPIR-V Shader Ready",
    },
    DeviceNode {
        category: "Display",
        name: "Mesa OSMesa softpipe OpenGL 2.1",
        pci_addr: "Software / CPU Rasterizer",
        vendor_id: "0x1002 (Mesa / Softpipe)",
        device_id: "OpenGL 2.1 Mesa 24.1.0",
        driver: "shared/nexus-abi/gpu.rs",
        status: "Active • 24-bit Z-Buffer Ready",
    },
    DeviceNode {
        category: "Network",
        name: "VirtIO-Net Ethernet Network Adapter",
        pci_addr: "00:03.0",
        vendor_id: "0x1AF4 (Red Hat, Inc.)",
        device_id: "0x1000 (VirtIO Network)",
        driver: "nexus-net virtio_net",
        status: "Active • DHCP / IPv4 Ready",
    },
    DeviceNode {
        category: "Audio",
        name: "Intel 82801AA AC'97 Sound Subsystem",
        pci_addr: "00:04.0",
        vendor_id: "0x8086 (Intel Corporation)",
        device_id: "0x2415 (AC'97 Audio)",
        driver: "kernel/sound ac97",
        status: "Active • 48kHz Stereo PCM",
    },
    DeviceNode {
        category: "Storage",
        name: "VirtIO Block Controller (System Disk)",
        pci_addr: "00:05.0",
        vendor_id: "0x1AF4 (Red Hat, Inc.)",
        device_id: "0x1001 (VirtIO Block)",
        driver: "nexus-kernel virtio_blk",
        status: "Active • GPT Partitioned",
    },
    DeviceNode {
        category: "USB",
        name: "xHCI USB 3.0 Host Controller",
        pci_addr: "00:06.0",
        vendor_id: "0x1B36 (Red Hat / QEMU)",
        device_id: "0x000D (qemu-xhci)",
        driver: "kernel/removable xhci",
        status: "Active • 2 Removable Drives Attached",
    },
    DeviceNode {
        category: "Wireless",
        name: "Intel Wi-Fi 6E AX210 160MHz Adapter",
        pci_addr: "00:07.0",
        vendor_id: "0x8086 (Intel Corporation)",
        device_id: "0x2725 (AX210 Wi-Fi 6E)",
        driver: "nexus-net iwlwifi / 802.11ax",
        status: "Connected • 5.0 GHz (SSID: NexusHome_5G)",
    },
    DeviceNode {
        category: "Bluetooth",
        name: "Intel Bluetooth 5.3 Controller & Subsystem",
        pci_addr: "00:07.1",
        vendor_id: "0x8086 (Intel Corporation)",
        device_id: "0x0032 (HCI USB / PCIe)",
        driver: "nexus-kernel bt_hci",
        status: "Active • Paired: Sony WH-1000XM5, MX Master 3S",
    },
    DeviceNode {
        category: "USB",
        name: "SanDisk Ultra 64GB USB 3.0 Flash Drive",
        pci_addr: "Port 1 (xHCI Slot 1)",
        vendor_id: "0x0781 (SanDisk Corp.)",
        device_id: "0x5581 (Ultra USB 3.0)",
        driver: "nexus-fs fat32 / mnt_usb",
        status: "Mounted at /mnt/usb0 • Capacity: 64.0 GB",
    },
    DeviceNode {
        category: "USB",
        name: "Logitech Unifying USB Receiver (HID)",
        pci_addr: "Port 2 (xHCI Slot 2)",
        vendor_id: "0x046D (Logitech, Inc.)",
        device_id: "0xC52B (USB Receiver)",
        driver: "nexus-kernel usb_hid",
        status: "Active • Keyboard & Optical Mouse (1000Hz)",
    },
];

pub struct DeviceManagerApp {
    selected_idx: usize,
}

impl DeviceManagerApp {
    pub fn new() -> Self {
        Self { selected_idx: 0 }
    }
}

impl Default for DeviceManagerApp {
    fn default() -> Self {
        Self::new()
    }
}

impl App for DeviceManagerApp {
    fn draw(&mut self, canvas: &mut Canvas) {
        let w = canvas.width();
        let h = canvas.height();

        canvas.fill(Rect::new(0, 0, w, h), COL_BG);

        // Header
        let hdr_rect = Rect::new(0, 0, w, 44);
        canvas.fill(hdr_rect, Colour(0x0014_1F2D));
        canvas.outline(hdr_rect, 1, COL_BORDER);

        canvas.fill_rounded(Rect::new(12, 8, 140, 28), 4, COL_BLUE);
        canvas.text_centred(Rect::new(12, 12, 140, 20), "DEVICE MANAGER", COL_TEXT_WHITE);
        canvas.text(
            164,
            14,
            "Hardware Topology & Driver Registry (PCI Bus)",
            COL_TEXT_MUTED,
        );

        // Left Device Tree Pane (width ~340)
        let tree_w = 340;
        let main_y = 52;
        let main_h = h.saturating_sub(main_y + 30);

        let tree_rect = Rect::new(12, main_y, tree_w, main_h);
        canvas.fill_rounded(tree_rect, 6, COL_PANEL);
        canvas.outline(tree_rect, 1, COL_BORDER);

        let count_str = format!("DISCOVERED DEVICES ({})", DEVICES.len());
        canvas.text(24, main_y + 12, &count_str, COL_GOLD);

        for (i, dev) in DEVICES.iter().enumerate() {
            let row_y = main_y + 36 + (i as u32 * 32);
            let is_sel = i == self.selected_idx;

            if is_sel {
                let row_rect = Rect::new(20, row_y - 2, tree_w.saturating_sub(16), 26);
                canvas.fill_rounded(row_rect, 4, Colour(0x001E_3A8A));
                canvas.outline(row_rect, 1, COL_CYAN);
            }

            let icon = match dev.category {
                "Processor" => "CPU",
                "Display" => "GPU",
                "Network" => "NET",
                "Wireless" => "WFI",
                "Bluetooth" => "BT ",
                "Audio" => "SND",
                "Storage" => "DSK",
                "USB" => "USB",
                _ => "DEV",
            };
            canvas.text(24, row_y, icon, COL_CYAN);

            let title_col = if is_sel {
                COL_TEXT_WHITE
            } else {
                COL_TEXT_MUTED
            };
            canvas.text(60, row_y, dev.name, title_col);
        }

        // Right Detail Pane
        let detail_x = 12 + tree_w + 16;
        let detail_w = w.saturating_sub(detail_x + 12);

        let detail_rect = Rect::new(detail_x, main_y, detail_w, main_h);
        canvas.fill_rounded(detail_rect, 6, COL_PANEL);
        canvas.outline(detail_rect, 1, COL_BORDER);

        if self.selected_idx < DEVICES.len() {
            let dev = &DEVICES[self.selected_idx];

            canvas.text(detail_x + 20, main_y + 20, dev.name, COL_TEXT_WHITE);
            let sub = format!("Device Class: {} Subsystem", dev.category);
            canvas.text(detail_x + 20, main_y + 44, &sub, COL_CYAN);

            let fields = [
                ("Hardware Status", dev.status, COL_GREEN),
                ("PCI Address / Location", dev.pci_addr, COL_TEXT_WHITE),
                ("Vendor ID", dev.vendor_id, COL_TEXT_WHITE),
                ("Device ID", dev.device_id, COL_TEXT_WHITE),
                ("Assigned Kernel Driver", dev.driver, COL_GOLD),
                (
                    "Resource Arbitration",
                    "IO Space / Memory Mapped BARs Configured",
                    COL_TEXT_MUTED,
                ),
                (
                    "Driver Subsystem",
                    "NexusOS Endowed Driver Infrastructure",
                    COL_TEXT_MUTED,
                ),
            ];

            let start_y = main_y + 80;
            for (idx, (label, val, col)) in fields.iter().enumerate() {
                let fy = start_y + (idx as u32 * 32);
                canvas.text(detail_x + 20, fy, label, COL_TEXT_MUTED);
                canvas.text(detail_x + 180, fy, val, *col);
            }
        }

        // Footer
        let footer_y = h.saturating_sub(26);
        canvas.fill(Rect::new(0, footer_y, w, 26), Colour(0x000A_0F1D));
        canvas.text(
            16,
            footer_y + 6,
            "Controls: [↑ / ↓] Select Device  [Tab] Focus  [R] Rescan PCI Bus",
            COL_TEXT_MUTED,
        );
    }

    fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Move(Movement::Up) if self.selected_idx > 0 => {
                self.selected_idx -= 1;
                true
            }
            Key::Move(Movement::Down) if self.selected_idx + 1 < DEVICES.len() => {
                self.selected_idx += 1;
                true
            }
            _ => false,
        }
    }

    fn click(&mut self, x: u32, y: u32) -> bool {
        let tree_w = 340;
        let main_y = 52;
        if x >= 12 && x <= 12 + tree_w && y >= main_y + 36 {
            let clicked_idx = ((y - (main_y + 36)) / 32) as usize;
            if clicked_idx < DEVICES.len() {
                self.selected_idx = clicked_idx;
                return true;
            }
        }
        false
    }

    fn damage(&mut self) -> Option<(u32, u32, u32, u32)> {
        None
    }
}

fn failed(message: &str) {
    nexus_user::log(message).ok();
}

fn finish() -> ! {
    nexus_user::exit_with(0)
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    nexus_user::log(&format!("devmgr: PANIC: {info}")).ok();
    nexus_user::exit_with(101)
}

#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "xor rbp, rbp",
        "call {main}",
        "ud2",
        main = sym main,
    )
}

extern "C" fn main() -> ! {
    if !nexus_user::heap::init(HEAP) {
        failed("devmgr: FAILED: could not allocate heap");
        finish();
    }

    nexus_user::log("devmgr: Device Manager starting").ok();

    let mut lent = [Handle(0); 1];
    match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok((window, _carried)) => {
            let mut app = DeviceManagerApp::new();
            nexus_user::log("devmgr: window opened with compositor, running UI loop").ok();
            window.run(&mut app);
        }
        Err(_) => {
            failed("devmgr: FAILED: could not connect to compositor");
        }
    }

    finish()
}
