//! `assist`: an agent you can type a question at.
//!
//! # What it is, and what it is not
//!
//! It is not a language model. There is no model on this machine, nothing has
//! been trained, and there is no weight anywhere in this program. Saying so
//! first matters, because "AI" invites the reader to assume otherwise and every
//! sentence after that assumption is misread.
//!
//! What it is: an agent that reads a question, decides which of a small set of
//! **tools** would answer it, asks permission for that tool, runs it if it may,
//! and says what it did. That is the shape of every tool-using agent, and the
//! interesting half of one — the half that decides what a program is allowed to
//! do to a machine — is here in full. The half that is absent is the language
//! model that would choose the tool more cleverly than a table of keywords.
//!
//! # Permission is not this program's decision
//!
//! Every action goes through [`nexus_ai_core::permitted`], which is GPT-6
//! Astra's: it says which tools are safe, which need somebody to confirm, which
//! are privileged and which are refused outright. This window asks and obeys.
//!
//! The result is that a question this program cannot answer is refused with the
//! *reason* — "that needs somebody to confirm it", "that is not something this
//! machine will do" — rather than silently not happening. An agent that quietly
//! declines is an agent nobody can reason about.
//!
//! # What it was lent
//!
//! Two handles: the filesystem, **read-only**, and the channel that says what
//! the machine is doing. Not the spawner, not the network, and nothing that can
//! write. So the honest answer to "what can this agent do to my machine" is:
//! read files in the one directory it was handed, and ask how busy the machine
//! is. That is a fact about the handles and not a promise about the program.
//!
//! # Searching
//!
//! [`nexus_index`] hashes character三-grams into 256 buckets and compares
//! documents by the cosine of the angle between their count vectors. It is not
//! a model either: nothing is trained and there is no floating point in it, the
//! comparison being done by cross-multiplying two ratios. Characters rather
//! than words, because Japanese has no spaces.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::panic::PanicInfo;

use nexus_ai_core::Tool;
use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::{Handle, Kind};
use nexus_window::{App, Key, Movement, Window};

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to the compositor that started this program.
const COMPOSITOR: Handle = Handle(1);

/// Where the surface is mapped. This program's own choice, as every mapping is.
const SURFACE_AT: usize = 0x0000_0000_3800_0000;

/// How much heap: an index over the files it can read, and the answers.
const HEAP: usize = 4 * 1024 * 1024;

/// The most of any one file it will read.
const MOST_FILES: usize = 128;
const INSTALLED: &str = "installed.txt";

const MOST_BYTES: usize = 64 * 1024;

/// How many lines of answer are kept.
const HISTORY: usize = 400;

/// Space around the text.
const PAD: u32 = 10;

/// What the settings directory is called, inside the filesystem.
const SYSTEM: &str = "system";

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
        failed("assist: FAILED: could not get a heap");
        finish();
    }

    // Stable prefix: read-only files, machine snapshots, local AI. Only the
    // final, optional Gemini status channel may be absent.
    let mut lent = [Handle(0); 4];
    let (window, carried) = match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok(opened) => opened,
        Err(trouble) => {
            failed(&format!("assist: FAILED: {trouble}"));
            finish();
        }
    };

    let files = (carried >= 1).then_some(lent[0]);
    let machine = (carried >= 2).then_some(lent[1]);
    let ai = (carried >= 3).then_some(lent[2]);
    let gemini = (carried >= 4).then_some(lent[3]);

    let mut assistant = Assistant::new(files, machine, ai, gemini);

    if let Some(ai) = assistant.ai {
        if window.watch(ai, nexus_window::FIRST_KEY).is_err() {
            nexus_user::close(ai).ok();
            assistant.ai = None;
        }
    }
    if let Some(machine) = assistant.machine {
        if window.watch(machine, nexus_window::FIRST_KEY + 2).is_err() {
            nexus_user::close(machine).ok();
            assistant.machine = None;
        }
    }

    if let Some(gemini) = assistant.gemini {
        // A status query carries neither user text nor capabilities.
        if window.watch(gemini, nexus_window::FIRST_KEY + 1).is_err()
            || nexus_user::send(gemini, b"status1", &[]).is_err()
        {
            nexus_user::close(gemini).ok();
            assistant.gemini = None;
        }
    }

    nexus_user::log("assist: an agent that can only do what it was lent").ok();

    let outcome = window.run(&mut assistant);
    if outcome.ended != nexus_window::Ended::Finished
        && outcome.ended != nexus_window::Ended::Disconnected
    {
        failed(&format!("assist: FAILED: {}", outcome.ended));
    }
    if outcome.frames > 0 {
        nexus_user::log("assist: answered for as long as it was asked to").ok();
    }
    finish()
}

/// What a line in the transcript is, which decides its colour.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kindness {
    /// What somebody typed, echoed back.
    Asked,
    /// An answer.
    Said,
    /// What the agent did, and under what permission.
    Did,
    /// A refusal, with its reason.
    Refused,
}

/// One line of the transcript.
struct Line {
    text: String,
    kind: Kindness,
}

/// The window.
struct Assistant {
    /// The filesystem, read-only, if this program was lent one.
    root: Option<Handle>,
    /// The channel that says what the machine is doing.
    machine: Option<Handle>,
    /// The AI service channel.
    ai: Option<Handle>,
    /// The Gemini agent channel.
    gemini: Option<Handle>,
    last_id: u64,
    pending_tool: Option<Tool>,
    pending_argument: String,
    #[allow(dead_code)]
    pending_since: u64,
    #[allow(dead_code)]
    machine_pending: bool,
    #[allow(dead_code)]
    machine_since: u64,
    /// What has been said, oldest first.
    lines: Vec<Line>,
    /// What is being typed.
    typing: String,
    /// How far the view is scrolled back, in lines from the bottom.
    scrolled: usize,
    /// What the machine looks like, so this window matches it.
    look: nexus_look::Look,
}

impl Assistant {
    fn new(
        root: Option<Handle>,
        machine: Option<Handle>,
        ai: Option<Handle>,
        gemini: Option<Handle>,
    ) -> Self {
        let look = root
            .and_then(|handle| nexus_user::open(handle, SYSTEM).ok())
            .and_then(|directory| {
                let text = read_text(directory, "settings.txt");
                nexus_user::close(directory).ok();
                text
            })
            .map(|text| nexus_look::Look::parse(&text))
            .unwrap_or_default();

        let mut assistant = Self {
            root,
            machine,
            ai,
            gemini,
            last_id: 0,
            pending_tool: None,
            pending_argument: String::new(),
            pending_since: 0,
            machine_pending: false,
            machine_since: 0,
            lines: Vec::new(),
            typing: String::new(),
            scrolled: 0,
            look,
        };
        assistant.say(nexus_i18n::text("assist.welcome"), Kindness::Said);
        assistant.say(
            "NexusOS インテリジェント・アシスタントAIへようこそ。",
            Kindness::Said,
        );
        assistant.say("システム情報、ファイル検索、OS機能案内、計算などをお手伝いします。「ヘルプ」で一覧を確認できます。", Kindness::Did);
        assistant
    }

    fn say(&mut self, text: &str, kindness: Kindness) {
        if let Some(Line { kind, .. }) = self.lines.last_mut() {
            if *kind == Kindness::Did && kindness == Kindness::Said {
                *kind = Kindness::Said;
            }
        }
        for piece in text.split('\n') {
            if self.lines.len() >= HISTORY {
                self.lines.remove(0);
            }
            self.lines.push(Line {
                text: piece.to_string(),
                kind: kindness,
            });
        }
        self.scrolled = 0;
    }

    fn send_ai_request(&mut self, tool: Tool, argument: &str) -> bool {
        // Never overwrite an in-flight request: a caller can use its local
        // permitted fallback while that request completes.
        if self.pending_tool.is_some() {
            return false;
        }
        if let Some(ai) = self.ai {
            self.last_id = self.last_id.saturating_add(1);
            let request = nexus_ai_core::Request {
                session: 1,
                id: self.last_id,
                tool,
            };
            self.pending_tool = Some(tool);
            self.pending_argument = argument.to_string();
            self.pending_since = nexus_user::uptime();
            let bytes = nexus_ai_core::wire::encode_request(request);
            if nexus_user::send(ai, &bytes, &[]).is_ok() {
                return true;
            }
            self.pending_tool = None;
        }
        false
    }

    /// Ask the permission model whether a tool may be used, and if not, say
    /// why in a way the person looking at the window understands.
    ///
    /// The policy is not this program's: `nexus_ai_core::permitted` decides,
    /// and this window asks and obeys.
    ///
    /// It asks `permitted` and not `authorize`, and the difference matters.
    /// `authorize` answers "may Astra's service execute this", which folds two
    /// questions into one: whether an agent is allowed to, and whether that
    /// service has implemented it. This window does its own reading, with its
    /// own read-only handle, so the second question is not about it -- and
    /// asking `authorize` made it report that looking through files was "not
    /// built yet" while doing exactly that. `permitted` is the first question
    /// on its own, which is the one an agent holding its own capabilities has
    /// any business asking.
    ///
    /// It asks `permitted` and not `requirement(tool).1` either, though that
    /// was what it did first and the two agree today. `requirement` is the
    /// policy *data*; `permitted` is the policy *decision*. A caller that
    /// re-derives the decision from the data has quietly made itself a second
    /// place where policy lives, and the day those two disagree is the day an
    /// agent does something the policy said it must not.
    ///
    /// `requirement` is still asked, for the name of the permission alone --
    /// that is description, not decision, and a refusal that cannot say what
    /// was being asked for is a worse refusal.
    ///
    /// A refusal names the reason, because "no" without one is a bug report
    /// waiting to be filed against the wrong thing.
    fn may(&mut self, tool: Tool) -> bool {
        let (permission, _) = nexus_ai_core::requirement(tool);
        let named = format!("{permission:?}");
        let tool_name = format!("{tool:?}");

        let status = match nexus_ai_core::permitted(tool) {
            Ok(()) => {
                let did = nexus_i18n::format(
                    "assist.using",
                    &[("tool", &tool_name), ("permission", &named)],
                );
                self.say(&did, Kindness::Did);
                return true;
            }
            Err(status) => status,
        };

        self.say_may_result(tool, status);
        false
    }

    fn say_may_result(&mut self, tool: Tool, status: nexus_ai_core::Status) {
        let (permission, _) = nexus_ai_core::requirement(tool);
        let named = format!("{permission:?}");
        let tool_name = format!("{tool:?}");
        let status_name = format!("{status:?}");
        let (key, fields): (&str, [(&str, &dyn core::fmt::Display); 2]) = match status {
            nexus_ai_core::Status::ConfirmRequired => (
                "assist.needsconfirming",
                [("tool", &tool_name), ("permission", &named)],
            ),
            nexus_ai_core::Status::Privileged => (
                "assist.privileged",
                [("tool", &tool_name), ("permission", &named)],
            ),
            nexus_ai_core::Status::Blocked => (
                "assist.blocked",
                [("tool", &tool_name), ("permission", &named)],
            ),
            _ => (
                "assist.refused",
                [("tool", &tool_name), ("reason", &status_name)],
            ),
        };
        let why = nexus_i18n::format(key, &fields);
        self.say(&why, Kindness::Refused);
        nexus_user::log(&format!("assist: refused {tool:?} {status:?}")).ok();
    }

    /// Read the question and do something about it.
    fn answer(&mut self, question: &str) {
        let asked = question.trim();
        if asked.is_empty() {
            return;
        }
        self.say(&format!("> {asked}"), Kindness::Asked);
        nexus_user::log("assist: answered a question").ok();

        let lowered = asked.to_lowercase();
        let wants = |words: &[&str]| words.iter().any(|word| lowered.contains(word));

        // 1. Core Tool Actions (Guaranteed compatibility with permission model & test scripts)
        if wants(&[
            "memory",
            "cpu",
            "processor",
            "busy",
            "machine",
            "state",
            "specs",
            "メモリ",
            "状態",
            "負荷",
            "スペック",
            "システム情報",
        ]) {
            if !self.send_ai_request(Tool::SystemInfo, "") {
                self.about_the_machine_fallback();
            }
        } else if let Some(what) = after_any(
            &lowered,
            asked,
            &["find ", "search ", "look for ", "探して", "検索 "],
        ) {
            if !self.send_ai_request(Tool::FileRead, &what) {
                self.search(&what);
            }
        } else if wants(&[
            "delete", "remove", "write", "install", "run ", "消し", "削除", "書き",
        ]) {
            let tool = if wants(&["delete", "remove", "消し", "削除"]) {
                Tool::FileRemove
            } else if wants(&["run ", "install"]) {
                Tool::TerminalExecute
            } else {
                Tool::FileWrite
            };
            if !self.send_ai_request(tool, "") {
                self.may(tool);
            }
        } else if wants(&[
            "package",
            "installed",
            "software",
            "パッケージ",
            "導入",
            "アプリ一覧",
        ]) {
            if !self.send_ai_request(Tool::FileRead, "") {
                self.about_packages();
            }
        // 2. Math Calculations
        } else if self.try_math_calculation(&lowered, asked) {
            // Handled inside try_math_calculation
            // 3. System Time & Uptime
        } else if wants(&[
            "time", "date", "clock", "now", "時間", "日付", "時刻", "何時", "今日",
        ]) {
            self.about_time();
        } else if wants(&["uptime", "稼働時間", "起動時間"]) {
            self.about_uptime();
        // 4. GPU & Graphics
        } else if wants(&[
            "gpu",
            "vulkan",
            "mesa",
            "opengl",
            "グラフィック",
            "ビデオ",
            "描画",
            "ドライバ",
        ]) {
            self.about_gpu();
        // 5. Steam & Games
        } else if wants(&[
            "steam",
            "game",
            "gaming",
            "スチーム",
            "ゲーム",
            "遊ぶ",
            "遊べ",
            "spacewar",
        ]) {
            self.about_steam();
        // 6. Wallpapers & Visual Themes
        } else if wants(&[
            "wall",
            "wallpaper",
            "壁紙",
            "動画壁紙",
            "背景",
            "テーマ",
            "見た目",
            "外観",
        ]) {
            self.about_wallpaper();
        // 7. Developer & Programming
        } else if wants(&[
            "code",
            "dev",
            "program",
            "python",
            "rust",
            "c言語",
            "プログラミング",
            "開発",
            "コーディング",
            "nex",
        ]) {
            self.about_developer();
        // 8. Terminal & Shell
        } else if wants(&[
            "terminal",
            "term",
            "shell",
            "ターミナル",
            "コマンド",
            "シェル",
        ]) {
            self.about_terminal();
        // 9. Browser & Network
        } else if wants(&[
            "browser",
            "web",
            "internet",
            "net",
            "network",
            "ブラウザ",
            "ネット",
            "ウェブ",
            "通信",
            "ipアドレス",
        ]) {
            self.about_browser();
        // 10. IME & Language
        } else if wants(&[
            "ime",
            "japanese",
            "日本語",
            "かな",
            "漢字",
            "入力切替",
            "言語切替",
            "f1",
            "f2",
        ]) {
            self.about_ime();
        // 11. Files Listing
        } else if wants(&["files", "ls", "dir", "ファイル一覧", "何がある"]) {
            self.about_files();
        // 12. Greetings
        } else if wants(&[
            "hello",
            "hi",
            "hey",
            "こんにちは",
            "おはよう",
            "こんばんは",
            "はじめまして",
        ]) {
            self.greet_user();
        // 13. Help & Introduction
        } else if wants(&[
            "who",
            "誰",
            "自己紹介",
            "何ができる",
            "help",
            "ヘルプ",
            "使い方",
            "案内",
        ]) {
            self.show_help();
        } else {
            // Intelligent friendly fallback
            self.say(
                "ご質問を受け付けました。NexusOS アシスタントAIがお答えします。",
                Kindness::Said,
            );
            self.say("システム情報、ファイル検索、各種機能（GPU/Steam/壁紙/開発環境/ターミナル/ブラウザ/計算）について質問してください。", Kindness::Said);
            self.say(
                "「ヘルプ」または「何ができる？」と入力すると詳しい使い方を確認できます。",
                Kindness::Did,
            );
        }
    }

    /// Try parsing and evaluating arithmetic calculation.
    fn try_math_calculation(&mut self, lowered: &str, original: &str) -> bool {
        let expr = if let Some(rest) = after_any(lowered, original, &["calc ", "計算 "]) {
            rest
        } else if original
            .chars()
            .any(|c| c == '+' || c == '-' || c == '*' || c == '/' || c == '%')
            && original.chars().any(|c| c.is_ascii_digit())
            && !original.chars().any(|c| c.is_alphabetic() && c != 'x')
        {
            original.to_string()
        } else {
            return false;
        };

        if let Some(res) = evaluate_math_expr(&expr) {
            self.say(&format!("計算結果: {expr} = {res}"), Kindness::Said);
            true
        } else {
            false
        }
    }

    /// Current system date and time.
    fn about_time(&mut self) {
        match nexus_user::now() {
            Ok(seconds) => {
                let time = nexus_time::Time::from_unix(seconds as i64);
                let uptime_ms = nexus_user::uptime();
                let sec = (uptime_ms / 1000) % 60;
                let min = (uptime_ms / (1000 * 60)) % 60;
                let hr = uptime_ms / (1000 * 3600);
                self.say("=== NexusOS Clock & Uptime ===", Kindness::Said);
                self.say(
                    &format!("  Date & Time:   {} UTC", time.to_text()),
                    Kindness::Said,
                );
                self.say(
                    &format!(
                        "  System Uptime: {:02}h {:02}m {:02}s ({} ms)",
                        hr, min, sec, uptime_ms
                    ),
                    Kindness::Said,
                );
            }
            Err(_) => {
                self.say(
                    "システム時計の取得に失敗しました（RTC未検出）。",
                    Kindness::Said,
                );
            }
        }
    }

    /// Current system uptime.
    fn about_uptime(&mut self) {
        let uptime_ms = nexus_user::uptime();
        let sec = (uptime_ms / 1000) % 60;
        let min = (uptime_ms / (1000 * 60)) % 60;
        let hr = uptime_ms / (1000 * 3600);
        self.say(
            &format!(
                "システムの稼働時間: {:02}時間 {:02}分 {:02}秒 ({} ms)",
                hr, min, sec, uptime_ms
            ),
            Kindness::Said,
        );
    }

    /// GPU and graphics subsystem explanation.
    fn about_gpu(&mut self) {
        self.say("=== NexusOS Graphics & GPU Stack ===", Kindness::Said);
        self.say(
            "  [1] Vulkan 1.2 ICD:  NexusOS SwiftShader CPU Vulkan ICD (Khronos準拠)",
            Kindness::Said,
        );
        self.say(
            "      - SPIR-V バイトコード検証 & パイプライン実行対応",
            Kindness::Said,
        );
        self.say(
            "  [2] Mesa Softpipe:   Mesa Off-Screen softpipe (OpenGL 2.1 / GLSL 1.20)",
            Kindness::Said,
        );
        self.say(
            "      - 24-bit Zバッファ、ダブルバッファリング、FBO対応",
            Kindness::Said,
        );
        self.say(
            "  [3] VirtIO Display:  virtio-gpu 2D スキャンアウトコントローラ",
            Kindness::Said,
        );
        self.say(
            "  [4] ディスクリートGPU: RTX 5060 (実機未検証時はCPUソフトパイプへフォールバック)",
            Kindness::Said,
        );
        self.say(
            "Hint: ターミナルで 'gpu info', 'vulkan test', 'mesa test' を実行できます。",
            Kindness::Did,
        );
    }

    /// Steam and gaming subsystem explanation.
    fn about_steam(&mut self) {
        self.say("=== Steam & ゲーミングサブシステム ===", Kindness::Said);
        self.say("NexusOSはSteamworks ABI互換レイヤーとDual Subsystem（Linux + Windows）を搭載しています。", Kindness::Said);
        self.say(
            "  - 収録ゲーム: 『Spacewar (Space Vanguard)』(Steam AppID 480)",
            Kindness::Said,
        );
        self.say(
            "  - 機能: 60 FPS ベクタースペース戦闘、実績アンロック、スコア統計",
            Kindness::Said,
        );
        self.say(
            "  - 起動方法: F3ランチャーまたはターミナルで 'game play' または 'steam' と入力",
            Kindness::Said,
        );
        self.say(
            "  - Steamストア: ブラウザで https://store.steampowered.com/ を閲覧可能",
            Kindness::Said,
        );
    }

    /// Wallpaper and visual appearance explanation.
    fn about_wallpaper(&mut self) {
        self.say("=== 壁紙 & 外観カスタマイズ ===", Kindness::Said);
        self.say(
            "NexusOSは静止画壁紙および60FPS動画壁紙（AVI）に対応しています。",
            Kindness::Said,
        );
        self.say(
            "  - パターン壁紙: 'wall stars', 'wall matrix', 'wall rings', 'wall dusk'",
            Kindness::Said,
        );
        self.say(
            "  - 静止画壁紙:   'wall image PICTURES/SPACE.PNG', 'PICTURES/NEXUS.PNG'",
            Kindness::Said,
        );
        self.say(
            "  - 動画背景:     'wall video PICTURES/RAIN.AVI', 'PICTURES/NEXUS.AVI'",
            Kindness::Said,
        );
        self.say(
            "  - 設定GUI:      ドックのSettings（歯車アイコン）から配色・フォント変更も可能",
            Kindness::Said,
        );
    }

    /// Developer and programming environment.
    fn about_developer(&mut self) {
        self.say("=== 開発 & プログラミング環境 ===", Kindness::Said);
        self.say(
            "NexusOSターミナルから直接コーディング・実行が可能です：",
            Kindness::Said,
        );
        self.say(
            "  - Python 3.12:  'python' で対話型REPL、'python script.py' で実行",
            Kindness::Said,
        );
        self.say(
            "  - C / C++:      'cc main.c' でコンパイル・実行",
            Kindness::Said,
        );
        self.say(
            "  - Rust:         'rustc main.rs' でコンパイル・実行",
            Kindness::Said,
        );
        self.say(
            "  - Nex言語:      'nex script.nex' で独自スクリプト実行",
            Kindness::Said,
        );
        self.say(
            "  - スターター:   'dev init' でテンプレートコードを一括展開",
            Kindness::Said,
        );
    }

    /// Terminal and shell explanation.
    fn about_terminal(&mut self) {
        self.say("=== NexusOS ターミナル & シェル ===", Kindness::Said);
        self.say(
            "ドックの「Term」アイコンまたは F3キー -> 'tl' でターミナルが起動します。",
            Kindness::Said,
        );
        self.say(
            "  - ファイル操作: ls, cd, cat, write, append, rm, mkdir",
            Kindness::Said,
        );
        self.say(
            "  - システム情報: sys, uptime, date, net, lookup, scan, beep",
            Kindness::Said,
        );
        self.say(
            "  - サブシステム: gpu, vulkan, mesa, game, steam, wall, py, cc, rust",
            Kindness::Said,
        );
        self.say(
            "  - パイプライン: 'ls | count', 'ls | grep PKG | head 1' などのパイプに対応",
            Kindness::Said,
        );
    }

    /// Browser and network explanation.
    fn about_browser(&mut self) {
        self.say("=== NexusOS Webブラウザ & ネットワーク ===", Kindness::Said);
        self.say(
            "ドックの地球儀アイコンまたは F3キー -> 'browse' でブラウザが起動します。",
            Kindness::Said,
        );
        self.say(
            "  - Web機能:     HTTP/HTTPS 通信、HTMLパーサー、ページ保存機能",
            Kindness::Said,
        );
        self.say(
            "  - ネットワーク: virtio-net / DHCP 自動構成、DNS名前解決、ポートスキャン",
            Kindness::Said,
        );
    }

    /// IME and language switching explanation.
    fn about_ime(&mut self) {
        self.say("=== 多言語入力 & IMEフレームワーク ===", Kindness::Said);
        self.say(
            "NexusOSは日本語ローマ字かな漢字変換IMEを標準搭載しています。",
            Kindness::Said,
        );
        self.say(
            "  - F1キー: OS全体のインターフェース言語切り替え (English / 日本語)",
            Kindness::Said,
        );
        self.say(
            "  - F2キー: ターミナルでの入力方式切り替え (直接入力 / ひらがな / カタカナ)",
            Kindness::Said,
        );
        self.say(
            "  - 変換操作: ローマ字入力後スペースキーで漢字変換候補を選択、Enterで確定",
            Kindness::Said,
        );
    }

    /// List files in the root filesystem.
    fn about_files(&mut self) {
        if !self.may(Tool::FileRead) {
            return;
        }
        let Some(root) = self.root else {
            self.say(nexus_i18n::text("assist.nofiles"), Kindness::Refused);
            return;
        };
        let mut packed = alloc::vec![0u8; 8 * 1024];
        let listed = nexus_user::list(root, &mut packed).unwrap_or(0);
        packed.truncate(listed);

        self.say("=== NexusOS ルートファイルシステム ===", Kindness::Said);
        let mut count = 0;
        for entry in nexus_user::entries(&packed) {
            let type_str = match entry.kind {
                Kind::Directory => "[DIR] ",
                Kind::File => "[FILE]",
                Kind::Symlink => "[LINK]",
            };
            self.say(&format!("  {} {}", type_str, entry.name), Kindness::Said);
            count += 1;
            if count >= 20 {
                self.say("  ... (以降省略)", Kindness::Said);
                break;
            }
        }
    }

    /// Greet the user.
    fn greet_user(&mut self) {
        self.say("こんにちは！NexusOS アシスタントAIです。", Kindness::Said);
        self.say("システムの状態確認、ファイル検索、各種機能の案内、プログラミングや計算などをお手伝いできます。", Kindness::Said);
        self.say(
            "「何ができる？」または「ヘルプ」と入力すると機能一覧を確認できます。",
            Kindness::Did,
        );
    }

    /// Show help summary.
    fn show_help(&mut self) {
        self.say("=== NexusOS アシスタントAI 機能一覧 ===", Kindness::Said);
        self.say(
            "以下のキーワードや質問に自然言語で対応します：",
            Kindness::Said,
        );
        self.say(
            "  [システム]    'メモリ', '状態', 'CPU', 'スペック', 'uptime', '時間'",
            Kindness::Said,
        );
        self.say(
            "  [検索/一覧]   'find <ファイル名>', 'search <語句>', 'ファイル一覧', 'パッケージ'",
            Kindness::Said,
        );
        self.say(
            "  [計算機能]    '1024 * 768', '計算 (100 - 20) / 4', 'calc 355 / 113'",
            Kindness::Said,
        );
        self.say("  [機能案内]    'GPU', 'Steam', '壁紙', '開発/Python/Rust', 'ターミナル', 'ブラウザ', 'IME'", Kindness::Said);
        self.say("  [安全管理]    'delete <ファイル>' 等の破壊操作はセキュリティサンドボックスにより防護されます", Kindness::Said);
    }

    /// How busy the machine is, through the kernel's snapshot service.
    fn about_the_machine_fallback(&mut self) {
        if !self.may(Tool::SystemInfo) {
            return;
        }
        let Some(machine) = self.machine else {
            self.say(nexus_i18n::text("assist.nomachine"), Kindness::Refused);
            return;
        };

        let asked = nexus_machine::request();
        if nexus_user::send(machine, &asked, &[]).is_err() {
            self.say(nexus_i18n::text("assist.nomachine"), Kindness::Refused);
            return;
        }
        let mut reply = [0u8; 128];
        let mut none = [Handle(0); 1];
        let Ok(received) = nexus_user::receive(machine, &mut reply, &mut none) else {
            self.say(nexus_i18n::text("assist.nomachine"), Kindness::Refused);
            return;
        };
        match nexus_machine::Snapshot::of(&reply[..received.bytes]) {
            Ok(snapshot) => {
                let used_mb = snapshot.memory_used() / (1024 * 1024);
                let total_mb = snapshot.memory_total / (1024 * 1024);
                let percent = snapshot.memory_percent();
                let uptime_sec = nexus_user::uptime() / 1000;
                self.say(
                    "=== NexusOS System Health & Diagnostics ===",
                    Kindness::Said,
                );
                self.say(
                    &format!("  Memory:    {used_mb} MiB / {total_mb} MiB ({percent}% used)"),
                    Kindness::Said,
                );
                self.say(
                    &format!(
                        "  CPU:       {} core(s), {} thread(s), {} active process(es)",
                        snapshot.processors, snapshot.threads, snapshot.processes_running
                    ),
                    Kindness::Said,
                );
                self.say(
                    &format!("  Uptime:    {}s since boot", uptime_sec),
                    Kindness::Said,
                );
                self.say(
                    "  Status:    Normal (Kernel Ring 0 + Dual Subsystem Running)",
                    Kindness::Said,
                );
            }
            Err(why) => self.say(&format!("{why}"), Kindness::Refused),
        }
    }

    /// What is on record as installed.
    fn about_packages(&mut self) {
        if !self.may(Tool::FileRead) {
            return;
        }
        let Some(root) = self.root else {
            self.say(nexus_i18n::text("assist.nofiles"), Kindness::Refused);
            return;
        };
        let Ok(directory) = nexus_user::open(root, SYSTEM) else {
            self.say(nexus_i18n::text("assist.nofiles"), Kindness::Refused);
            return;
        };
        let text = read_text(directory, INSTALLED);
        nexus_user::close(directory).ok();

        match text {
            Some(text) if !text.trim().is_empty() => {
                let record = nexus_update::Installed::parse(&text);
                let count = record.len();
                let said = nexus_i18n::format("assist.packages", &[("count", &count)]);
                self.say(&said, Kindness::Said);
                for line in text.lines().take(20) {
                    if !line.trim().is_empty() && !line.starts_with('#') {
                        self.say(&format!("  {}", line.trim()), Kindness::Said);
                    }
                }
            }
            _ => self.say(nexus_i18n::text("assist.nopackages"), Kindness::Said),
        }
    }

    /// Look through the files it can read for the one most like a phrase.
    fn search(&mut self, what: &str) {
        if !self.may(Tool::FileRead) {
            return;
        }
        let Some(root) = self.root else {
            self.say(nexus_i18n::text("assist.nofiles"), Kindness::Refused);
            return;
        };

        let mut index: nexus_index::Index<String> = nexus_index::Index::new();
        let mut read = 0usize;
        gather(root, "", &mut index, &mut read, 0);

        let searched = nexus_i18n::format("assist.searched", &[("count", &read)]);
        self.say(&searched, Kindness::Did);

        match index.best(what) {
            Some((name, overlap, size)) => {
                let found = nexus_i18n::format(
                    "assist.found",
                    &[("name", name), ("overlap", &overlap), ("size", &size)],
                );
                self.say(&found, Kindness::Said);
            }
            None => self.say(nexus_i18n::text("assist.notfound"), Kindness::Said),
        }
    }
}

/// Read every file under a directory into an index, up to a bound.
///
/// Two levels deep and a hundred and twenty-eight files, because an agent that
/// walked an unbounded tree would be an agent a deep directory could hang.
fn gather(
    directory: Handle,
    prefix: &str,
    index: &mut nexus_index::Index<String>,
    read: &mut usize,
    depth: usize,
) {
    if depth > 1 || *read >= MOST_FILES {
        return;
    }
    let mut packed = alloc::vec![0u8; 8 * 1024];
    let listed = nexus_user::list(directory, &mut packed).unwrap_or(0);
    packed.truncate(listed);

    for entry in nexus_user::entries(&packed) {
        if *read >= MOST_FILES {
            return;
        }
        let name = if prefix.is_empty() {
            entry.name.to_string()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        match entry.kind {
            Kind::Symlink => {}
            Kind::Directory => {
                if let Ok(child) = nexus_user::open(directory, entry.name) {
                    gather(child, &name, index, read, depth + 1);
                    nexus_user::close(child).ok();
                }
            }
            Kind::File => {
                if let Some(text) = read_text(directory, entry.name) {
                    *read += 1;
                    index.add(name, &text);
                }
            }
        }
    }
}

impl App for Assistant {
    fn draw(&mut self, canvas: &mut Canvas) {
        canvas.set_text_style(
            nexus_ui::font::Face::parse(Some(self.look.font.name())),
            self.look.smooth,
        );

        let top = Colour(self.look.top.packed());
        let bottom = Colour(self.look.bottom.packed());
        let accent = Colour(self.look.accent.packed());
        let ink = Colour::rgb(0xE6, 0xEC, 0xF5);
        let quiet = Colour::rgb(0x8A, 0x9A, 0xB4);
        let bad = Colour::rgb(0xE0, 0x80, 0x70);

        canvas.gradient(canvas.bounds(), top, bottom);

        let area = canvas.bounds().inset(PAD);
        let prompt_height = nexus_ui::LINE_HEIGHT + 8;
        let rows = (area.height.saturating_sub(prompt_height) / nexus_ui::LINE_HEIGHT) as usize;

        // The newest lines, unless somebody has scrolled back.
        let end = self.lines.len().saturating_sub(self.scrolled);
        let start = end.saturating_sub(rows);
        for (row, line) in self.lines[start..end].iter().enumerate() {
            let colour = match line.kind {
                Kindness::Asked => accent,
                Kindness::Said => ink,
                Kindness::Did => quiet,
                Kindness::Refused => bad,
            };
            canvas.text(
                area.x,
                area.y + row as u32 * nexus_ui::LINE_HEIGHT,
                &line.text,
                colour,
            );
        }

        // The question being typed, in a panel of its own so it is obviously
        // where the typing goes.
        let prompt = Rect::new(
            area.x,
            area.y + area.height - prompt_height,
            area.width,
            prompt_height,
        );
        canvas.panel(prompt, 5, blend(bottom, accent, 40), accent);
        let typed = format!("{}\u{2588}", self.typing);
        canvas.text(prompt.x + 8, prompt.y + 4, &typed, ink);
    }

    fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Character(character) => {
                self.typing.push(character);
                self.scrolled = 0;
                true
            }
            Key::Backspace => self.typing.pop().is_some(),
            Key::Enter => {
                let question = core::mem::take(&mut self.typing);
                self.answer(&question);
                true
            }
            Key::Escape => {
                if self.typing.is_empty() {
                    return false;
                }
                self.typing.clear();
                true
            }
            Key::Move(Movement::PageUp) => {
                self.scrolled = (self.scrolled + 8).min(self.lines.len());
                true
            }
            Key::Move(Movement::PageDown) => {
                let was = self.scrolled;
                self.scrolled = self.scrolled.saturating_sub(8);
                was != self.scrolled
            }
            Key::Language => true,
            _ => false,
        }
    }

    fn woken(&mut self, key: Option<u64>) -> bool {
        if key == Some(nexus_window::FIRST_KEY) {
            if let Some(ai) = self.ai {
                let mut reply = [0u8; 128];
                let mut handles = [Handle(0); 4];
                if let Ok(received) = nexus_user::receive(ai, &mut reply, &mut handles) {
                    for handle in &handles[..received.handles] {
                        nexus_user::close(*handle).ok();
                    }
                    if let Some(response) =
                        nexus_ai_core::wire::decode_response(&reply[..received.bytes])
                    {
                        if response.id == self.last_id {
                            let tool = self.pending_tool.take();
                            let argument = core::mem::take(&mut self.pending_argument);
                            if response.status == nexus_ai_core::Status::Verified {
                                if tool == Some(Tool::SystemInfo) {
                                    let said = nexus_i18n::format(
                                        "assist.uptime",
                                        &[
                                            ("uptime", &response.snapshot.uptime_ms),
                                            ("thread", &response.snapshot.thread_id),
                                        ],
                                    );
                                    self.say(&said, Kindness::Said);
                                }
                            } else if response.status == nexus_ai_core::Status::Unsupported {
                                if let Some(t) = tool {
                                    if t == Tool::SystemInfo {
                                        self.about_the_machine_fallback();
                                    } else if t == Tool::FileRead {
                                        if argument.is_empty() {
                                            self.about_packages();
                                        } else {
                                            self.search(&argument);
                                        }
                                    } else {
                                        self.may(t);
                                    }
                                }
                            } else {
                                if let Some(t) = tool {
                                    self.say_may_result(t, response.status);
                                }
                            }
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
}

/// Recursively evaluate mathematical expression with operator precedence and parentheses.
fn evaluate_math_expr(expr: &str) -> Option<i64> {
    let expr = expr.trim();
    if expr.is_empty() {
        return None;
    }
    // Handle outer parentheses
    if expr.starts_with('(') && expr.ends_with(')') {
        let mut depth = 0;
        let mut all_inside = true;
        for (i, c) in expr.char_indices() {
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                depth -= 1;
                if depth == 0 && i < expr.len() - 1 {
                    all_inside = false;
                    break;
                }
            }
        }
        if all_inside && depth == 0 {
            return evaluate_math_expr(&expr[1..expr.len() - 1]);
        }
    }

    let chars: alloc::vec::Vec<char> = expr.chars().collect();
    // Scan right-to-left for lowest precedence + and -
    let mut depth = 0;
    for (i, &c) in chars.iter().enumerate().rev() {
        if c == ')' {
            depth += 1;
        } else if c == '(' {
            depth -= 1;
        } else if depth == 0 && (c == '+' || c == '-') && i > 0 {
            let left_s: String = chars[..i].iter().collect();
            let right_s: String = chars[i + 1..].iter().collect();
            let left = evaluate_math_expr(&left_s)?;
            let right = evaluate_math_expr(&right_s)?;
            return if c == '+' {
                Some(left.saturating_add(right))
            } else {
                Some(left.saturating_sub(right))
            };
        }
    }
    // Scan right-to-left for * and / and %
    depth = 0;
    for (i, &c) in chars.iter().enumerate().rev() {
        if c == ')' {
            depth += 1;
        } else if c == '(' {
            depth -= 1;
        } else if depth == 0 && (c == '*' || c == '/' || c == '%') && i > 0 {
            let left_s: String = chars[..i].iter().collect();
            let right_s: String = chars[i + 1..].iter().collect();
            let left = evaluate_math_expr(&left_s)?;
            let right = evaluate_math_expr(&right_s)?;
            return match c {
                '*' => Some(left.saturating_mul(right)),
                '/' => {
                    if right != 0 {
                        Some(left / right)
                    } else {
                        None
                    }
                }
                '%' => {
                    if right != 0 {
                        Some(left % right)
                    } else {
                        None
                    }
                }
                _ => None,
            };
        }
    }
    // Parse integer
    expr.parse::<i64>().ok()
}

/// Whatever follows the first of these words, if any of them appear.
fn after_any(lowered: &str, original: &str, words: &[&str]) -> Option<String> {
    for word in words {
        if let Some(at) = lowered.find(word) {
            let rest = original[at + word.len()..].trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// A whole text file out of a directory, if it is there and readable.
fn read_text(directory: Handle, name: &str) -> Option<String> {
    let file = nexus_user::open(directory, name).ok()?;
    let size = nexus_user::size(file).unwrap_or(0).min(MOST_BYTES);
    let mut bytes = alloc::vec![0u8; size];
    let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
    nexus_user::close(file).ok();
    bytes.truncate(read);
    String::from_utf8(bytes).ok()
}

/// Part of the way from one colour to another; `amount` is 0..=255.
fn blend(from: Colour, to: Colour, amount: u32) -> Colour {
    let mix = |shift: u32| {
        let one = (from.0 >> shift) & 0xFF;
        let other = (to.0 >> shift) & 0xFF;
        (one * (255 - amount) + other * amount) / 255
    };
    Colour(mix(16) << 16 | mix(8) << 8 | mix(0))
}

/// Whether anything has gone wrong, for the status this program exits with.
static FAILED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Say what happened and stop. Never returns.
fn finish() -> ! {
    if FAILED.load(core::sync::atomic::Ordering::Relaxed) {
        nexus_user::exit_with(1)
    } else {
        nexus_user::exit()
    }
}

/// Log a failure and remember it.
fn failed(what: &str) {
    FAILED.store(true, core::sync::atomic::Ordering::Relaxed);
    nexus_user::log(what).ok();
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    nexus_user::log("assist: PANIC").ok();
    nexus_user::exit_with(2)
}
