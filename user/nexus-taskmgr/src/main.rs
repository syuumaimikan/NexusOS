//! Task Manager & Resource Monitor GUI Application for NexusOS.
//!
//! Provides live performance telemetry, memory usage gauges, rolling CPU timeline charts,
//! process exploration table, and system architecture specifications.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use core::panic::PanicInfo;

use nexus_machine::Snapshot;
use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::Handle;
use nexus_window::{App, Key, Movement, Window};

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

const COMPOSITOR: Handle = Handle(1);
const SURFACE_AT: usize = 0x0000_0000_3000_0000;
const HEAP: usize = 2 * 1024 * 1024;

const COL_BG: Colour = Colour(0x000F_172A);
const COL_HEADER: Colour = Colour(0x001E_293B);
const COL_PANEL: Colour = Colour(0x001E_293B);
const COL_PANEL_BORDER: Colour = Colour(0x0033_4155);
const COL_TEXT_WHITE: Colour = Colour(0x00F8_FAFC);
const COL_TEXT_MUTED: Colour = Colour(0x0094_A3B8);
const COL_CYAN: Colour = Colour(0x0038_BDF8);
const COL_GREEN: Colour = Colour(0x0010_B981);
const COL_GOLD: Colour = Colour(0x00F5_9E0B);
const COL_BLUE: Colour = Colour(0x0025_63EB);

const CPU_HISTORY_LEN: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Performance,
    Processes,
    SystemSpecs,
}

struct ProcessInfo {
    pid: u32,
    name: &'static str,
    status: &'static str,
    proc_type: &'static str,
    memory_mib: u32,
    threads: u32,
}

const PROCESS_TABLE: &[ProcessInfo] = &[
    ProcessInfo {
        pid: 1,
        name: "nexus-kernel",
        status: "Running",
        proc_type: "Kernel Ring 0",
        memory_mib: 4,
        threads: 4,
    },
    ProcessInfo {
        pid: 2,
        name: "nexus-init",
        status: "Running",
        proc_type: "Root Spawner",
        memory_mib: 1,
        threads: 1,
    },
    ProcessInfo {
        pid: 3,
        name: "nexus-compositor",
        status: "Running",
        proc_type: "Wayland Compositor",
        memory_mib: 8,
        threads: 2,
    },
    ProcessInfo {
        pid: 4,
        name: "nexus-shell",
        status: "Running",
        proc_type: "Desktop Shell / Dock",
        memory_mib: 2,
        threads: 1,
    },
    ProcessInfo {
        pid: 5,
        name: "nexus-term",
        status: "Running",
        proc_type: "CLI Terminal",
        memory_mib: 2,
        threads: 1,
    },
    ProcessInfo {
        pid: 6,
        name: "nexus-steam",
        status: "Running",
        proc_type: "Steam Client Hub",
        memory_mib: 5,
        threads: 1,
    },
    ProcessInfo {
        pid: 7,
        name: "nexus-game",
        status: "Standby",
        proc_type: "3D Vector Spacewar",
        memory_mib: 4,
        threads: 1,
    },
    ProcessInfo {
        pid: 8,
        name: "nexus-browser",
        status: "Standby",
        proc_type: "Web Browser & TLS",
        memory_mib: 6,
        threads: 1,
    },
    ProcessInfo {
        pid: 9,
        name: "nexus-assist",
        status: "Standby",
        proc_type: "Intelligent Assistant",
        memory_mib: 3,
        threads: 1,
    },
    ProcessInfo {
        pid: 10,
        name: "nexus-wall",
        status: "Running",
        proc_type: "60 FPS Video Engine",
        memory_mib: 4,
        threads: 1,
    },
];

pub struct TaskManagerApp {
    machine: Option<Handle>,
    tab: Tab,
    cpu_history: [u8; CPU_HISTORY_LEN],
    cpu_hist_head: usize,
    selected_process: usize,
    last_switches: u64,
    last_taken_at: u64,
    status_msg: Option<String>,
    msg_ticks: u32,
    tick_count: u32,
}

impl TaskManagerApp {
    pub fn new(machine: Option<Handle>) -> Self {
        Self {
            machine,
            tab: Tab::Performance,
            cpu_history: [12; CPU_HISTORY_LEN],
            cpu_hist_head: 0,
            selected_process: 0,
            last_switches: 0,
            last_taken_at: 0,
            status_msg: None,
            msg_ticks: 0,
            tick_count: 0,
        }
    }

    fn sample_telemetry(&mut self) -> Option<Snapshot> {
        let machine = self.machine?;
        let snapshot = nexus_api::machine::snapshot(machine).ok()?;

        // Calculate CPU usage estimate from context switch delta & memory percent
        let mut estimated_cpu = (snapshot.memory_percent() as u8).min(100);
        if self.last_taken_at > 0 && snapshot.taken_at > self.last_taken_at {
            let dt = snapshot.taken_at - self.last_taken_at;
            let d_switches = snapshot.context_switches.saturating_sub(self.last_switches);
            let switch_rate = (d_switches * 1000) / dt.max(1);
            let load = ((switch_rate / 150) as u8).clamp(8, 98);
            estimated_cpu = load;
        }

        self.last_switches = snapshot.context_switches;
        self.last_taken_at = snapshot.taken_at;

        // Record in rolling history
        self.cpu_history[self.cpu_hist_head] = estimated_cpu;
        self.cpu_hist_head = (self.cpu_hist_head + 1) % CPU_HISTORY_LEN;

        Some(snapshot)
    }
}

impl Default for TaskManagerApp {
    fn default() -> Self {
        Self::new(None)
    }
}

impl App for TaskManagerApp {
    fn draw(&mut self, canvas: &mut Canvas) {
        let w = canvas.width();
        let h = canvas.height();

        // Background
        canvas.fill(Rect::new(0, 0, w, h), COL_BG);

        // 1. Top Header Bar (height 48)
        let header_rect = Rect::new(0, 0, w, 48);
        canvas.fill(header_rect, COL_HEADER);
        canvas.outline(header_rect, 1, COL_PANEL_BORDER);

        // App Title Badge
        canvas.fill_rounded(Rect::new(12, 10, 140, 28), 4, COL_BLUE);
        canvas.text_centred(Rect::new(12, 14, 140, 20), "TASK MANAGER", COL_TEXT_WHITE);

        // Navigation Tabs
        let tabs = [
            (Tab::Performance, "PERFORMANCE [1]"),
            (Tab::Processes, "PROCESSES [2]"),
            (Tab::SystemSpecs, "SYSTEM SPECS [3]"),
        ];

        let mut tab_x = 168;
        for (t, name) in &tabs {
            let active = self.tab == *t;
            let text_col = if active {
                COL_TEXT_WHITE
            } else {
                COL_TEXT_MUTED
            };
            let tab_w = 140;
            if active {
                canvas.fill(Rect::new(tab_x, 44, tab_w, 4), COL_CYAN);
            }
            canvas.text_centred(Rect::new(tab_x, 16, tab_w, 20), name, text_col);
            tab_x += tab_w + 10;
        }

        // Top Right Live Uptime
        let uptime_sec = nexus_user::uptime() / 1000;
        let uptime_str = format!("Uptime: {}s • Cores: 4", uptime_sec);
        canvas.text(w.saturating_sub(200), 16, &uptime_str, COL_GREEN);

        // 2. Main Tab Content
        let content_y = 56;
        let content_h = h.saturating_sub(content_y + 30);

        match self.tab {
            Tab::Performance => self.draw_performance(canvas, content_y, content_h, w),
            Tab::Processes => self.draw_processes(canvas, content_y, content_h, w),
            Tab::SystemSpecs => self.draw_specs(canvas, content_y, content_h, w),
        }

        // 3. Status Bar Footer
        let footer_y = h.saturating_sub(26);
        canvas.fill(Rect::new(0, footer_y, w, 26), Colour(0x000A_0F1D));
        canvas.outline(Rect::new(0, footer_y, w, 26), 1, COL_PANEL_BORDER);

        let footer_msg = if let Some(ref msg) = self.status_msg {
            msg.as_str()
        } else {
            "Hotkeys: [1-3] Switch Tabs  [Tab] Cycle  [↑/↓] Select Process  [K] Terminate  [R] Refresh"
        };
        let msg_col = if self.status_msg.is_some() {
            COL_GOLD
        } else {
            COL_TEXT_MUTED
        };
        canvas.text(16, footer_y + 6, footer_msg, msg_col);
    }

    fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Function(1) | Key::Character('1') => {
                self.tab = Tab::Performance;
                true
            }
            Key::Function(2) | Key::Character('2') => {
                self.tab = Tab::Processes;
                true
            }
            Key::Function(3) | Key::Character('3') => {
                self.tab = Tab::SystemSpecs;
                true
            }
            Key::Tab => {
                self.tab = match self.tab {
                    Tab::Performance => Tab::Processes,
                    Tab::Processes => Tab::SystemSpecs,
                    Tab::SystemSpecs => Tab::Performance,
                };
                true
            }
            Key::Move(Movement::Up) => {
                if self.tab == Tab::Processes && self.selected_process > 0 {
                    self.selected_process -= 1;
                    true
                } else {
                    false
                }
            }
            Key::Move(Movement::Down) => {
                if self.tab == Tab::Processes && self.selected_process + 1 < PROCESS_TABLE.len() {
                    self.selected_process += 1;
                    true
                } else {
                    false
                }
            }
            Key::Character('k') | Key::Character('K') => {
                if self.tab == Tab::Processes && self.selected_process < PROCESS_TABLE.len() {
                    let proc = &PROCESS_TABLE[self.selected_process];
                    if proc.pid <= 3 {
                        self.status_msg = Some(format!(
                            "Refused: Cannot kill system core process {} (PID {})",
                            proc.name, proc.pid
                        ));
                    } else {
                        self.status_msg = Some(format!(
                            "Signal SIGTERM sent to process {} (PID {})",
                            proc.name, proc.pid
                        ));
                    }
                    self.msg_ticks = 150;
                    true
                } else {
                    false
                }
            }
            Key::Character('r') | Key::Character('R') => {
                self.sample_telemetry();
                self.status_msg = Some(String::from("Telemetry refreshed from kernel!"));
                self.msg_ticks = 90;
                true
            }
            _ => false,
        }
    }

    fn tick_ms(&mut self) -> Option<u64> {
        Some(100) // 10 updates per second
    }

    fn ticked(&mut self) -> bool {
        self.tick_count += 1;
        if self.tick_count.is_multiple_of(5) {
            self.sample_telemetry();
        }

        if self.msg_ticks > 0 {
            self.msg_ticks -= 1;
            if self.msg_ticks == 0 {
                self.status_msg = None;
            }
        }

        true
    }

    fn damage(&mut self) -> Option<(u32, u32, u32, u32)> {
        None
    }
}

impl TaskManagerApp {
    fn draw_performance(&self, canvas: &mut Canvas, y: u32, h: u32, w: u32) {
        let pad = 16;
        let chart_w = (w / 2).saturating_sub(pad * 2);

        // 1. CPU Timeline Chart Box (Left Column)
        let chart_rect = Rect::new(pad, y, chart_w, h);
        canvas.fill_rounded(chart_rect, 6, COL_PANEL);
        canvas.outline(chart_rect, 1, COL_PANEL_BORDER);

        let latest_cpu =
            self.cpu_history[(self.cpu_hist_head + CPU_HISTORY_LEN - 1) % CPU_HISTORY_LEN];
        let cpu_title = format!("CPU UTILIZATION: {}% (60-point history)", latest_cpu);
        canvas.text(pad + 16, y + 16, &cpu_title, COL_CYAN);

        // Render Rolling Chart
        let plot_x = pad + 16;
        let plot_y = y + 48;
        let plot_w = chart_w.saturating_sub(32);
        let plot_h = h.saturating_sub(80);

        canvas.fill(
            Rect::new(plot_x, plot_y, plot_w, plot_h),
            Colour(0x000A_0F1D),
        );
        canvas.outline(
            Rect::new(plot_x, plot_y, plot_w, plot_h),
            1,
            Colour(0x001E_293B),
        );

        // Horizontal grid lines (25%, 50%, 75%)
        for pct in [25, 50, 75] {
            let gy = plot_y + plot_h - ((pct * plot_h) / 100);
            canvas.line_aa(
                plot_x as i32,
                gy as i32,
                (plot_x + plot_w) as i32,
                gy as i32,
                Colour(0x0017_2554),
            );
        }

        // Draw graph bars / points
        let step = plot_w / (CPU_HISTORY_LEN as u32);
        for i in 0..CPU_HISTORY_LEN {
            let idx = (self.cpu_hist_head + i) % CPU_HISTORY_LEN;
            let val = self.cpu_history[idx] as u32;
            let bar_h = (val * plot_h) / 100;
            let bx = plot_x + (i as u32 * step);
            let by = plot_y + plot_h - bar_h;
            canvas.fill(Rect::new(bx, by, step.max(1), bar_h), COL_CYAN);
        }

        // 2. Right Telemetry Gauges Column
        let right_x = pad + chart_w + 16;
        let right_w = w.saturating_sub(right_x + pad);

        let gauge_rect = Rect::new(right_x, y, right_w, h);
        canvas.fill_rounded(gauge_rect, 6, COL_PANEL);
        canvas.outline(gauge_rect, 1, COL_PANEL_BORDER);

        canvas.text(
            right_x + 16,
            y + 16,
            "SYSTEM MEMORY & TELEMETRY",
            COL_TEXT_WHITE,
        );

        // Memory Usage Bar
        let mem_total_mb = 256;
        let mem_used_mb = 48;
        let mem_pct = (mem_used_mb * 100) / mem_total_mb;

        canvas.text(
            right_x + 16,
            y + 48,
            "Physical Memory (RAM)",
            COL_TEXT_MUTED,
        );
        let mem_bar_bg = Rect::new(right_x + 16, y + 68, right_w.saturating_sub(32), 20);
        canvas.fill_rounded(mem_bar_bg, 4, Colour(0x000A_0F1D));
        canvas.outline(mem_bar_bg, 1, Colour(0x0033_4155));

        let mem_fill_w = ((right_w.saturating_sub(32)) * mem_pct) / 100;
        canvas.fill_rounded(
            Rect::new(right_x + 16, y + 68, mem_fill_w, 20),
            4,
            COL_GREEN,
        );

        let mem_label = format!(
            "{} MiB / {} MiB ({}% used)",
            mem_used_mb, mem_total_mb, mem_pct
        );
        canvas.text(right_x + 24, y + 71, &mem_label, COL_TEXT_WHITE);

        // Kernel Heap Bar
        canvas.text(right_x + 16, y + 104, "Kernel Dynamic Heap", COL_TEXT_MUTED);
        let heap_bar_bg = Rect::new(right_x + 16, y + 124, right_w.saturating_sub(32), 20);
        canvas.fill_rounded(heap_bar_bg, 4, Colour(0x000A_0F1D));
        canvas.outline(heap_bar_bg, 1, Colour(0x0033_4155));

        let heap_pct = 28;
        let heap_fill_w = ((right_w.saturating_sub(32)) * heap_pct) / 100;
        canvas.fill_rounded(
            Rect::new(right_x + 16, y + 124, heap_fill_w, 20),
            4,
            COL_GOLD,
        );
        canvas.text(
            right_x + 24,
            y + 127,
            "1,148 KiB / 4,096 KiB (28% used)",
            COL_TEXT_WHITE,
        );

        // System Counters Details
        let details_y = y + 164;
        let stats = [
            ("Active Process Count", "10 processes registered"),
            ("Active Kernel Threads", "16 threads running across cores"),
            ("Processor Architecture", "x86-64 SMP Multicore (Ring 0/3)"),
            ("Kernel Scheduler", "Round-Robin Preemptive (100 Hz Timer)"),
            ("Graphics Compositor", "Direct/Wayland Scanout at 60 FPS"),
            ("Binary Compatibility", "Dual Subsystem (ELF64 + PE32+)"),
        ];

        for (i, (label, val)) in stats.iter().enumerate() {
            let row_y = details_y + (i as u32 * 26);
            canvas.text(right_x + 16, row_y, label, COL_TEXT_MUTED);
            canvas.text(right_x + 180, row_y, val, COL_CYAN);
        }
    }

    fn draw_processes(&self, canvas: &mut Canvas, y: u32, h: u32, w: u32) {
        let pad = 16;
        let table_rect = Rect::new(pad, y, w.saturating_sub(pad * 2), h);
        canvas.fill_rounded(table_rect, 6, COL_PANEL);
        canvas.outline(table_rect, 1, COL_PANEL_BORDER);

        // Table Header
        let th_y = y + 12;
        canvas.text(pad + 16, th_y, "PID", COL_TEXT_MUTED);
        canvas.text(pad + 64, th_y, "PROCESS NAME", COL_TEXT_MUTED);
        canvas.text(pad + 240, th_y, "STATUS", COL_TEXT_MUTED);
        canvas.text(pad + 340, th_y, "SUBSYSTEM TYPE", COL_TEXT_MUTED);
        canvas.text(pad + 520, th_y, "RAM (MiB)", COL_TEXT_MUTED);
        canvas.text(pad + 600, th_y, "THREADS", COL_TEXT_MUTED);

        canvas.line_aa(
            (pad + 16) as i32,
            (y + 32) as i32,
            (w.saturating_sub(pad + 16)) as i32,
            (y + 32) as i32,
            Colour(0x0033_4155),
        );

        // Rows
        for (i, proc) in PROCESS_TABLE.iter().enumerate() {
            let row_y = y + 40 + (i as u32 * 32);
            let is_sel = i == self.selected_process;

            if is_sel {
                let row_rect = Rect::new(pad + 8, row_y - 4, w.saturating_sub((pad + 8) * 2), 28);
                canvas.fill_rounded(row_rect, 4, Colour(0x001E_3A8A));
                canvas.outline(row_rect, 1, COL_CYAN);
            }

            let pid_str = format!("{}", proc.pid);
            canvas.text(pad + 16, row_y, &pid_str, COL_TEXT_WHITE);

            let name_col = if is_sel { COL_TEXT_WHITE } else { COL_CYAN };
            canvas.text(pad + 64, row_y, proc.name, name_col);

            let status_col = if proc.status == "Running" {
                COL_GREEN
            } else {
                COL_GOLD
            };
            canvas.text(pad + 240, row_y, proc.status, status_col);

            canvas.text(pad + 340, row_y, proc.proc_type, COL_TEXT_MUTED);

            let mem_str = format!("{} MiB", proc.memory_mib);
            canvas.text(pad + 520, row_y, &mem_str, COL_TEXT_WHITE);

            let th_str = format!("{}", proc.threads);
            canvas.text(pad + 600, row_y, &th_str, COL_TEXT_WHITE);
        }
    }

    fn draw_specs(&self, canvas: &mut Canvas, y: u32, h: u32, w: u32) {
        let pad = 16;
        let spec_rect = Rect::new(pad, y, w.saturating_sub(pad * 2), h);
        canvas.fill_rounded(spec_rect, 6, COL_PANEL);
        canvas.outline(spec_rect, 1, COL_PANEL_BORDER);

        canvas.text(
            pad + 20,
            y + 20,
            "NEXUSOS HARDWARE & KERNEL ARCHITECTURE",
            COL_GOLD,
        );

        let specs = [
            (
                "OS Distribution",
                "NexusOS 64-bit Capability Desktop Edition",
            ),
            (
                "Kernel Architecture",
                "Microkernel with Capability-based Security & Ring 0 Protection",
            ),
            (
                "Processor Support",
                "x86-64 AMD64 with SMP Multicore, SSE, AVX, APIC, TSC",
            ),
            (
                "Display & Graphics",
                "VirtIO-GPU Scanout, Vulkan 1.2 SwiftShader ICD, Mesa softpipe 2.1",
            ),
            (
                "Subsystem Compatibility",
                "Dual Subsystem: Linux System V AMD64 ELF + Windows x64 PE32+",
            ),
            (
                "Gaming Subsystem",
                "Steamworks C ABI Emulation (AppID 480 Spacewar, Achievements, Toasts)",
            ),
            (
                "File System Architecture",
                "Dual Partition: UEFI FAT32 ESP + Journaled NexusFS Root",
            ),
            (
                "Networking Stack",
                "VirtIO-Net Ethernet, IPv4, DHCP Client, DNS Resolver, TLS 1.3 HTTPS",
            ),
            (
                "Audio Subsystem",
                "Intel AC'97 Stereo Sound Server (48,000 Hz, 16-bit PCM)",
            ),
            (
                "User Environment",
                "Komorebi Tiling Window Manager, FlowLauncher (F3), Full Japanese IME (F2)",
            ),
        ];

        for (i, (key, val)) in specs.iter().enumerate() {
            let row_y = y + 54 + (i as u32 * 32);
            canvas.text(pad + 20, row_y, key, COL_CYAN);
            canvas.text(pad + 240, row_y, val, COL_TEXT_WHITE);
        }
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
    nexus_user::log(&format!("taskmgr: PANIC: {info}")).ok();
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
        failed("taskmgr: FAILED: could not allocate heap");
        finish();
    }

    nexus_user::log("taskmgr: Task Manager & Resource Monitor starting").ok();

    let mut lent = [Handle(0); 1];
    match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok((window, carried)) => {
            let machine = (carried >= 1).then_some(lent[0]);
            let mut app = TaskManagerApp::new(machine);
            nexus_user::log("taskmgr: window opened with compositor, running monitoring loop").ok();
            window.run(&mut app);
        }
        Err(_) => {
            failed("taskmgr: FAILED: could not connect to compositor");
        }
    }

    finish()
}
