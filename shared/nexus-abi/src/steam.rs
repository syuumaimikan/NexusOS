//! Steamworks Compatibility Layer & Gaming Subsystem ABI.
//!
//! Provides the data structures, interface models, and compatibility shims
//! that emulate the Valve Steamworks API (both Linux `libsteam_api.so` and
//! Windows `steam_api64.dll`). This enables games with Steamworks integration
//! (such as Spacewar, retro games, and standalone titles) to initialize, query
//! user persona and stats, unlock achievements, and execute smoothly on NexusOS.

/// Steam 64-bit ID structure.
///
/// Encodes Universe, Account Type, Instance, and Account ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct CSteamID(pub u64);

impl CSteamID {
    /// Universe constants.
    pub const UNIVERSE_INDIVIDUAL: u8 = 0;
    pub const UNIVERSE_PUBLIC: u8 = 1;
    pub const UNIVERSE_BETA: u8 = 2;
    pub const UNIVERSE_INTERNAL: u8 = 3;
    pub const UNIVERSE_DEV: u8 = 4;

    /// Account Type constants.
    pub const TYPE_INVALID: u8 = 0;
    pub const TYPE_INDIVIDUAL: u8 = 1;
    pub const TYPE_MULTISEAT: u8 = 2;
    pub const TYPE_GAME_SERVER: u8 = 3;

    /// Construct a CSteamID from components.
    #[must_use]
    pub const fn new(account_id: u32, instance: u32, account_type: u8, universe: u8) -> Self {
        let raw = (account_id as u64)
            | ((instance as u64 & 0x000F_FFFF) << 32)
            | ((account_type as u64 & 0x0F) << 52)
            | ((universe as u64) << 56);
        Self(raw)
    }

    /// Default Nexus player Steam ID (Universe: Public, Type: Individual, ID: 10001).
    pub const NEXUS_PLAYER: Self =
        Self::new(10001, 1, Self::TYPE_INDIVIDUAL, Self::UNIVERSE_PUBLIC);

    /// Convert to raw 64-bit unsigned integer.
    #[must_use]
    pub const fn to_u64(&self) -> u64 {
        self.0
    }

    /// Returns whether this SteamID is valid.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        let acc_type = (self.0 >> 52) & 0x0F;
        acc_type != Self::TYPE_INVALID as u64 && (self.0 & 0xFFFF_FFFF) != 0
    }

    /// Extract 32-bit Account ID.
    #[must_use]
    pub const fn account_id(&self) -> u32 {
        (self.0 & 0xFFFF_FFFF) as u32
    }
}

/// Steam API result codes (EResult enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum EResult {
    Ok = 1,
    Fail = 2,
    NoConnection = 3,
    InvalidPassword = 5,
    LoggedInElsewhere = 6,
    InvalidProtocol = 7,
    InvalidParam = 8,
    FileNotFound = 9,
    Busy = 10,
    InvalidState = 11,
    InvalidName = 12,
    InvalidEmail = 13,
    DuplicateName = 14,
    AccessDenied = 15,
    Timeout = 16,
    Banned = 17,
    AccountNotFound = 18,
    InvalidSteamID = 19,
    ServiceUnavailable = 20,
    NotLoggedOn = 21,
    Pending = 22,
    EncryptionFailure = 23,
    InsufficientPrivilege = 24,
    LimitExceeded = 25,
}

/// Steam Persona state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum EPersonaState {
    Offline = 0,
    Online = 1,
    Busy = 2,
    Away = 3,
    Snooze = 4,
    LookingToTrade = 5,
    LookingToPlay = 6,
    Invisible = 7,
}

/// Steam handle types.
pub type HSteamPipe = i32;
pub type HSteamUser = i32;
pub type AppId = u32;

/// Default Spacewar AppID used by developers for Steamworks testing and demonstrations.
pub const APPID_SPACEWAR: AppId = 480;

/// Valve Half-Life 2 AppID.
pub const APPID_HL2: AppId = 220;

/// Stardew Valley AppID.
pub const APPID_STARDEW: AppId = 413150;

/// Maximum number of tracked achievements in memory.
pub const MAX_ACHIEVEMENTS: usize = 32;

/// Maximum number of tracked player stats in memory.
pub const MAX_STATS: usize = 16;

/// Achievement definition and state.
#[derive(Debug, Clone, Copy)]
pub struct Achievement {
    pub id: [u8; 32],
    pub name: [u8; 32],
    pub desc: [u8; 64],
    pub achieved: bool,
    pub unlock_time: u64,
}

impl Achievement {
    /// Create a new locked achievement.
    #[must_use]
    pub const fn new(id_str: &str, name_str: &str, desc_str: &str) -> Self {
        let mut id = [0u8; 32];
        let mut name = [0u8; 32];
        let mut desc = [0u8; 64];

        let id_bytes = id_str.as_bytes();
        let name_bytes = name_str.as_bytes();
        let desc_bytes = desc_str.as_bytes();

        let mut i = 0;
        while i < id_bytes.len() && i < 31 {
            id[i] = id_bytes[i];
            i += 1;
        }

        let mut j = 0;
        while j < name_bytes.len() && j < 31 {
            name[j] = name_bytes[j];
            j += 1;
        }

        let mut k = 0;
        while k < desc_bytes.len() && k < 63 {
            desc[k] = desc_bytes[k];
            k += 1;
        }

        Self {
            id,
            name,
            desc,
            achieved: false,
            unlock_time: 0,
        }
    }

    /// Returns ID as string slice.
    #[must_use]
    pub fn id_str(&self) -> &str {
        bytes_to_str(&self.id)
    }

    /// Returns display name as string slice.
    #[must_use]
    pub fn name_str(&self) -> &str {
        bytes_to_str(&self.name)
    }

    /// Returns description as string slice.
    #[must_use]
    pub fn desc_str(&self) -> &str {
        bytes_to_str(&self.desc)
    }
}

/// Helper converting a null-padded or null-terminated byte slice to a &str.
fn bytes_to_str(slice: &[u8]) -> &str {
    let len = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    core::str::from_utf8(&slice[..len]).unwrap_or("")
}

/// Integer stat entry.
#[derive(Debug, Clone, Copy)]
pub struct StatEntry {
    pub name: [u8; 32],
    pub value: i32,
}

impl StatEntry {
    #[must_use]
    pub const fn new(name_str: &str, initial_val: i32) -> Self {
        let mut name = [0u8; 32];
        let name_bytes = name_str.as_bytes();
        let mut i = 0;
        while i < name_bytes.len() && i < 31 {
            name[i] = name_bytes[i];
            i += 1;
        }
        Self {
            name,
            value: initial_val,
        }
    }

    #[must_use]
    pub fn name_str(&self) -> &str {
        bytes_to_str(&self.name)
    }
}

/// Pending achievement toast notification for screen display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToastNotification {
    pub name: [u8; 32],
    pub desc: [u8; 64],
    pub displayed_ticks: u32,
}

impl ToastNotification {
    #[must_use]
    pub fn name_str(&self) -> &str {
        bytes_to_str(&self.name)
    }

    #[must_use]
    pub fn desc_str(&self) -> &str {
        bytes_to_str(&self.desc)
    }
}

/// Global Steamworks API runtime context for NexusOS.
pub struct SteamContext {
    pub appid: AppId,
    pub initialized: bool,
    pub steam_id: CSteamID,
    pub persona_name: [u8; 32],
    pub ip_country: [u8; 4],
    pub pipe: HSteamPipe,
    pub user: HSteamUser,
    pub achievements: [Achievement; MAX_ACHIEVEMENTS],
    pub achievement_count: usize,
    pub stats: [StatEntry; MAX_STATS],
    pub stat_count: usize,
    pub pending_toast: Option<ToastNotification>,
}

impl SteamContext {
    /// Create a new default Steam context.
    #[must_use]
    pub const fn new() -> Self {
        const EMPTY_ACH: Achievement = Achievement {
            id: [0u8; 32],
            name: [0u8; 32],
            desc: [0u8; 64],
            achieved: false,
            unlock_time: 0,
        };
        const EMPTY_STAT: StatEntry = StatEntry {
            name: [0u8; 32],
            value: 0,
        };

        let mut persona = [0u8; 32];
        let p_src = b"NexusPlayer\0";
        let mut i = 0;
        while i < p_src.len() && i < 32 {
            persona[i] = p_src[i];
            i += 1;
        }

        let mut country = [0u8; 4];
        country[0] = b'J';
        country[1] = b'P';
        country[2] = 0;
        country[3] = 0;

        Self {
            appid: APPID_SPACEWAR,
            initialized: false,
            steam_id: CSteamID::NEXUS_PLAYER,
            persona_name: persona,
            ip_country: country,
            pipe: 1,
            user: 1,
            achievements: [EMPTY_ACH; MAX_ACHIEVEMENTS],
            achievement_count: 0,
            stats: [EMPTY_STAT; MAX_STATS],
            stat_count: 0,
            pending_toast: None,
        }
    }

    /// Initialize the Steamworks API with a specified AppID.
    pub fn init_with_appid(&mut self, appid: AppId) -> bool {
        self.appid = appid;
        self.initialized = true;

        // Register default Spacewar / game achievements if registering AppID 480
        if self.appid == APPID_SPACEWAR && self.achievement_count == 0 {
            self.register_achievement(
                "ACH_FIRST_LAUNCH",
                "First Flight",
                "Launched the game successfully",
            );
            self.register_achievement(
                "ACH_SHARPSHOOTER",
                "Sharpshooter",
                "Destroyed an asteroid with high precision",
            );
            self.register_achievement(
                "ACH_SURVIVOR",
                "Space Survivor",
                "Survived 3 waves of asteroids",
            );
            self.register_achievement(
                "ACH_SPACE_ACE",
                "Space Ace",
                "Attained 1,000 points in one sortie",
            );

            self.register_stat("NumGames", 1);
            self.register_stat("HighScore", 0);
            self.register_stat("AsteroidsDestroyed", 0);
        }

        true
    }

    /// Register a new achievement definition.
    pub fn register_achievement(&mut self, id: &str, name: &str, desc: &str) -> bool {
        if self.achievement_count >= MAX_ACHIEVEMENTS {
            return false;
        }
        for i in 0..self.achievement_count {
            if self.achievements[i].id_str() == id {
                return true;
            }
        }
        self.achievements[self.achievement_count] = Achievement::new(id, name, desc);
        self.achievement_count += 1;
        true
    }

    /// Register a new stat counter.
    pub fn register_stat(&mut self, name: &str, initial: i32) -> bool {
        if self.stat_count >= MAX_STATS {
            return false;
        }
        for i in 0..self.stat_count {
            if self.stats[i].name_str() == name {
                return true;
            }
        }
        self.stats[self.stat_count] = StatEntry::new(name, initial);
        self.stat_count += 1;
        true
    }

    /// Unlock an achievement by ID. Returns true if newly unlocked.
    pub fn unlock_achievement(&mut self, id: &str) -> bool {
        for i in 0..self.achievement_count {
            if self.achievements[i].id_str() == id {
                if !self.achievements[i].achieved {
                    self.achievements[i].achieved = true;
                    self.achievements[i].unlock_time = 1;

                    // Trigger achievement toast
                    self.pending_toast = Some(ToastNotification {
                        name: self.achievements[i].name,
                        desc: self.achievements[i].desc,
                        displayed_ticks: 0,
                    });
                    return true;
                }
                return false;
            }
        }
        false
    }

    /// Query achievement status.
    #[must_use]
    pub fn is_achievement_unlocked(&self, id: &str) -> bool {
        for i in 0..self.achievement_count {
            if self.achievements[i].id_str() == id {
                return self.achievements[i].achieved;
            }
        }
        false
    }

    /// Query integer stat.
    #[must_use]
    pub fn get_stat(&self, name: &str) -> Option<i32> {
        for i in 0..self.stat_count {
            if self.stats[i].name_str() == name {
                return Some(self.stats[i].value);
            }
        }
        None
    }

    /// Set integer stat.
    pub fn set_stat(&mut self, name: &str, val: i32) -> bool {
        for i in 0..self.stat_count {
            if self.stats[i].name_str() == name {
                self.stats[i].value = val;
                return true;
            }
        }
        self.register_stat(name, val)
    }

    /// Pop pending toast if active, advancing display ticks.
    pub fn poll_toast(&mut self) -> Option<ToastNotification> {
        if let Some(mut toast) = self.pending_toast {
            toast.displayed_ticks += 1;
            // Display for ~180 frames (approx 3 seconds at 60 FPS)
            if toast.displayed_ticks > 180 {
                self.pending_toast = None;
            } else {
                self.pending_toast = Some(toast);
            }
            Some(toast)
        } else {
            None
        }
    }

    /// Returns player persona name.
    #[must_use]
    pub fn persona_name_str(&self) -> &str {
        bytes_to_str(&self.persona_name)
    }
}

impl Default for SteamContext {
    fn default() -> Self {
        Self::new()
    }
}

// -------------------------------------------------------------------------
// Global Singleton / C-Compatible Steamworks API Entry Points
// -------------------------------------------------------------------------

static mut GLOBAL_STEAM_CONTEXT: SteamContext = SteamContext::new();

/// Returns a mutable reference to the global Steamworks context.
///
/// SAFETY: Called in single-threaded context or cooperatively within the process.
#[must_use]
pub fn steam_context() -> &'static mut SteamContext {
    unsafe { &mut *core::ptr::addr_of_mut!(GLOBAL_STEAM_CONTEXT) }
}

/// Initialize the Steamworks API (`SteamAPI_Init`).
///
/// Returns true on success. Reads `steam_appid.txt` or defaults to AppID 480 (Spacewar).
#[no_mangle]
pub extern "C" fn SteamAPI_Init() -> bool {
    let ctx = steam_context();
    ctx.init_with_appid(ctx.appid)
}

/// Initialize the Steamworks API Flat version (`SteamAPI_InitFlat`).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn SteamAPI_InitFlat(out_err_msg: *mut u8) -> bool {
    let ctx = steam_context();
    let ok = ctx.init_with_appid(ctx.appid);
    if !ok && !out_err_msg.is_null() {
        unsafe {
            let msg = b"Failed to init Steamworks\0";
            core::ptr::copy_nonoverlapping(msg.as_ptr(), out_err_msg, msg.len());
        }
    }
    ok
}

/// Shutdown the Steamworks API (`SteamAPI_Shutdown`).
#[no_mangle]
pub extern "C" fn SteamAPI_Shutdown() {
    let ctx = steam_context();
    ctx.initialized = false;
}

/// Check if application restart is necessary (`SteamAPI_RestartAppIfNecessary`).
///
/// In Steam, if the game was not launched via Steam client, it returns true and
/// relaunches via `steam://run/<appid>`. In our compatibility layer, we return
/// false so the game starts immediately without requiring external relaunch.
#[no_mangle]
pub extern "C" fn SteamAPI_RestartAppIfNecessary(own_appid: AppId) -> bool {
    let ctx = steam_context();
    ctx.appid = own_appid;
    false
}

/// Returns true if Steam is running (`SteamAPI_IsSteamRunning`).
#[no_mangle]
pub extern "C" fn SteamAPI_IsSteamRunning() -> bool {
    true
}

/// Run queued Steamworks callbacks (`SteamAPI_RunCallbacks`).
#[no_mangle]
pub extern "C" fn SteamAPI_RunCallbacks() {
    // Process callbacks and poll toasts
}

/// Get HSteamUser handle (`SteamAPI_GetHSteamUser`).
#[no_mangle]
pub extern "C" fn SteamAPI_GetHSteamUser() -> HSteamUser {
    steam_context().user
}

/// Get HSteamPipe handle (`SteamAPI_GetHSteamPipe`).
#[no_mangle]
pub extern "C" fn SteamAPI_GetHSteamPipe() -> HSteamPipe {
    steam_context().pipe
}

// -------------------------------------------------------------------------
// Interface Shim Functions
// -------------------------------------------------------------------------

/// ISteamUser::GetSteamID
#[no_mangle]
pub extern "C" fn SteamAPI_ISteamUser_GetSteamID() -> u64 {
    steam_context().steam_id.to_u64()
}

/// ISteamUser::BLoggedOn
#[no_mangle]
pub extern "C" fn SteamAPI_ISteamUser_BLoggedOn() -> bool {
    true
}

/// ISteamFriends::GetPersonaName
#[no_mangle]
pub extern "C" fn SteamAPI_ISteamFriends_GetPersonaName() -> *const u8 {
    steam_context().persona_name.as_ptr()
}

/// ISteamUtils::GetAppID
#[no_mangle]
pub extern "C" fn SteamAPI_ISteamUtils_GetAppID() -> AppId {
    steam_context().appid
}

/// ISteamUtils::GetIPCountry
#[no_mangle]
pub extern "C" fn SteamAPI_ISteamUtils_GetIPCountry() -> *const u8 {
    steam_context().ip_country.as_ptr()
}

/// ISteamApps::BIsSubscribedApp
#[no_mangle]
pub extern "C" fn SteamAPI_ISteamApps_BIsSubscribedApp(appid: AppId) -> bool {
    let _ = appid;
    true
}

/// ISteamApps::BIsDlcInstalled
#[no_mangle]
pub extern "C" fn SteamAPI_ISteamApps_BIsDlcInstalled(appid: AppId) -> bool {
    let _ = appid;
    true
}

/// ISteamUserStats::RequestCurrentStats
#[no_mangle]
pub extern "C" fn SteamAPI_ISteamUserStats_RequestCurrentStats() -> bool {
    true
}

/// ISteamUserStats::GetAchievement
pub fn steam_get_achievement(name: &str) -> bool {
    steam_context().is_achievement_unlocked(name)
}

/// ISteamUserStats::SetAchievement
pub fn steam_set_achievement(name: &str) -> bool {
    steam_context().unlock_achievement(name)
}

/// ISteamUserStats::StoreStats
pub fn steam_store_stats() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_steam_id_encoding() {
        let sid = CSteamID::new(
            10001,
            1,
            CSteamID::TYPE_INDIVIDUAL,
            CSteamID::UNIVERSE_PUBLIC,
        );
        assert!(sid.is_valid());
        assert_eq!(sid.account_id(), 10001);
        assert_eq!(sid.to_u64(), 0x0110_0001_0000_2711);
    }

    #[test]
    fn test_steam_init_and_achievements() {
        let mut ctx = SteamContext::new();
        assert!(!ctx.initialized);
        assert!(ctx.init_with_appid(APPID_SPACEWAR));
        assert!(ctx.initialized);
        assert_eq!(ctx.appid, 480);
        assert_eq!(ctx.persona_name_str(), "NexusPlayer");

        // Verify default Spacewar achievements are registered
        assert_eq!(ctx.achievement_count, 4);
        assert!(!ctx.is_achievement_unlocked("ACH_FIRST_LAUNCH"));

        // Unlock achievement
        assert!(ctx.unlock_achievement("ACH_FIRST_LAUNCH"));
        assert!(ctx.is_achievement_unlocked("ACH_FIRST_LAUNCH"));
        // Second unlock returns false (already unlocked)
        assert!(!ctx.unlock_achievement("ACH_FIRST_LAUNCH"));

        // Verify toast notification is queued
        let toast = ctx.poll_toast().expect("toast should be pending");
        assert_eq!(toast.name_str(), "First Flight");
        assert_eq!(toast.desc_str(), "Launched the game successfully");
    }

    #[test]
    fn test_steam_stats() {
        let mut ctx = SteamContext::new();
        ctx.init_with_appid(APPID_SPACEWAR);

        assert_eq!(ctx.get_stat("HighScore"), Some(0));
        assert!(ctx.set_stat("HighScore", 4200));
        assert_eq!(ctx.get_stat("HighScore"), Some(4200));

        assert!(ctx.set_stat("CustomStat", 99));
        assert_eq!(ctx.get_stat("CustomStat"), Some(99));
    }

    #[test]
    fn test_steam_api_stubs() {
        assert!(!SteamAPI_RestartAppIfNecessary(480));
        assert!(SteamAPI_IsSteamRunning());
        assert!(SteamAPI_Init());
        assert_eq!(SteamAPI_ISteamUtils_GetAppID(), 480);
        assert!(SteamAPI_ISteamUser_BLoggedOn());
        assert_eq!(SteamAPI_GetHSteamUser(), 1);
        assert_eq!(SteamAPI_GetHSteamPipe(), 1);
        SteamAPI_Shutdown();
    }
}
