use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use chrono::{DateTime, Duration, TimeZone, Utc};
use machine_uid;
use rand::Rng;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;
#[cfg(target_os = "windows")]
use winreg::enums::*;
#[cfg(target_os = "windows")]
use winreg::RegKey;

// CONFIGURACION LEMONSQUEEZY
const LEMON_API_URL: &str = "https://api.lemonsqueezy.com/v1/licenses/activate";
const LEMON_VALIDATE_URL: &str = "https://api.lemonsqueezy.com/v1/licenses/validate";
const LICENSE_FILE_NAME: &str = "app_data.enc";
const SHADOW_DIR_NAME: &str = ".zenith_astro";
const SHADOW_FILE_NAME: &str = "license_shadow.dat";
#[cfg(target_os = "windows")]
const REGISTRY_PATH: &str = "Software\\ZenithAstroStacker";
const APP_SALT: &str = "ZenithAstroStacker_Salt_V1_Secured";
const TRIAL_DURATION_SEC: i64 = 15 * 24 * 3600;
const FREE_DURATION_SEC: i64 = 15 * 24 * 3600;
const REMOTE_RECHECK_INTERVAL_SEC: i64 = 6 * 3600;
const RENEWAL_GRACE_CHECK_WINDOW_SEC: i64 = 3 * 24 * 3600;
#[cfg(target_os = "windows")]
const ADS_FILE_NAME: &str = "sys_config_cache.dat";
#[cfg(target_os = "windows")]
const ADS_STREAM_NAME: &str = "secure_license_stream";

#[derive(Serialize, Deserialize, Debug, Clone)]
struct LocalLicenseState {
    install_date: i64,
    last_check_date: i64,
    license_key: Option<String>,
    instance_id: Option<String>,
    is_pro_verified: bool,
    has_used_trial: bool,
    license_type: Option<String>, // "TRIAL", "ANNUAL", "FREE" or legacy "PRO"/"PERMANENT"
    #[serde(default)]
    activation_date: Option<i64>,
    signature: String,
    #[serde(default)]
    expires_at: Option<String>, // ISO8601 string from API
}

#[derive(Serialize, Debug, Clone)]
pub struct AppStatus {
    pub status: String,
    pub days_remaining: i64,
    pub is_pro: bool,
    pub message: String,
    pub license_key: Option<String>,
    pub license_type: String,
    pub registered_at: Option<String>,
    pub expires_at: Option<String>,
    pub last_checked_at: Option<String>,
    pub renewal_status: Option<String>,
}

#[derive(Deserialize, Debug)]
struct LemonResponse {
    activated: bool,
    error: Option<String>,
    license_key: Option<LemonKeyInfo>,
    instance: Option<LemonInstanceInfo>,
    meta: Option<LemonMeta>,
}

#[derive(Deserialize, Debug)]
struct LemonKeyInfo {
    key: Option<String>,
    expires_at: Option<String>, // API Field for Expiry
    status: Option<String>,
}

#[derive(Deserialize, Debug)]
struct LemonValidateResponse {
    valid: bool,
    license_key: Option<LemonKeyInfo>,
    meta: Option<LemonMeta>,
}

#[derive(Deserialize, Debug)]
struct LemonMeta {
    variant_name: Option<String>,
    product_name: Option<String>,
}

#[derive(Deserialize, Debug)]
struct LemonInstanceInfo {
    id: String,
}

#[derive(Debug)]
struct RemoteValidation {
    is_valid: bool,
    expires_at: Option<String>,
    license_type: Option<String>,
}

pub struct LicenseManager {
    app_dir: PathBuf,
    state: Mutex<LocalLicenseState>,
}

impl LicenseManager {
    pub fn new(app_dir: PathBuf) -> Self {
        let manager = LicenseManager {
            app_dir: app_dir.clone(),
            state: Mutex::new(LocalLicenseState {
                install_date: 0,
                last_check_date: 0,
                license_key: None,
                instance_id: None,
                is_pro_verified: false,
                has_used_trial: false,
                license_type: None,
                activation_date: None,
                signature: String::new(),
                expires_at: None,
            }),
        };

        manager.initialize();
        manager
    }

    fn get_encryption_key() -> Key<Aes256Gcm> {
        let machine_id = machine_uid::get().unwrap_or_else(|_| "generic_fallback_uid".to_string());
        let combined = format!("{}{}", machine_id, APP_SALT);
        let mut hasher = Sha256::new();
        hasher.update(combined.as_bytes());
        let result = hasher.finalize();
        *Key::<Aes256Gcm>::from_slice(&result)
    }

    // Ejecutar reqwest::blocking en un hilo separado
    fn get_secure_now(&self) -> i64 {
        let result = thread::spawn(|| {
            let client = Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .ok()?;
            if let Ok(resp) = client.head("https://www.google.com").send() {
                if let Some(date_header) = resp.headers().get("date") {
                    if let Ok(date_str) = date_header.to_str() {
                        if let Ok(parsed) = DateTime::parse_from_rfc2822(date_str) {
                            return Some(parsed.with_timezone(&Utc).timestamp());
                        }
                    }
                }
            }
            None
        })
        .join();

        if let Ok(Some(timestamp)) = result {
            timestamp
        } else {
            Utc::now().timestamp()
        }
    }

    fn parse_expiry(exp_str: &str) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(exp_str)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }

    fn format_timestamp(ts: i64) -> Option<String> {
        if ts <= 0 {
            return None;
        }
        Utc.timestamp_opt(ts, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
    }

    fn format_expiry(exp_str: &str) -> String {
        Self::parse_expiry(exp_str)
            .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| exp_str.to_string())
    }

    fn mask_license_key(key: &str) -> String {
        let compact: String = key.chars().filter(|c| !c.is_whitespace()).collect();
        let tail: String = compact
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        if tail.is_empty() {
            "Sin clave".to_string()
        } else {
            format!("••••-••••-••••-{}", tail)
        }
    }

    fn normalized_type(license_type: Option<&str>) -> String {
        match license_type.unwrap_or("PRO").to_uppercase().as_str() {
            "PERMANENT" => "PRO".to_string(),
            "ANUAL" => "ANNUAL".to_string(),
            "YEARLY" => "ANNUAL".to_string(),
            other => other.to_string(),
        }
    }

    fn has_full_access_type(license_type: &str) -> bool {
        matches!(license_type, "PRO" | "ANNUAL" | "TRIAL")
    }

    fn display_license_type(license_type: &str) -> String {
        match license_type {
            "ANNUAL" => "Licencia anual".to_string(),
            "TRIAL" => "Prueba gratuita".to_string(),
            "FREE" => "Licencia gratuita".to_string(),
            "PRO" => "Licencia PRO".to_string(),
            _ => "Licencia".to_string(),
        }
    }

    fn detect_license_type(expires_at: Option<&str>, meta: Option<&LemonMeta>) -> String {
        let names = meta
            .map(|m| {
                format!(
                    "{} {}",
                    m.variant_name.clone().unwrap_or_default(),
                    m.product_name.clone().unwrap_or_default()
                )
                .to_lowercase()
            })
            .unwrap_or_default();

        if names.contains("trial")
            || names.contains("prueba")
            || names.contains("evaluacion")
            || names.contains("evaluación")
            || names.contains("15")
        {
            return "TRIAL".to_string();
        }

        if names.contains("free")
            || names.contains("gratis")
            || names.contains("gratuita")
            || names.contains("gratuito")
        {
            return "FREE".to_string();
        }

        if names.contains("annual")
            || names.contains("anual")
            || names.contains("yearly")
            || names.contains("subscription")
            || names.contains("suscripcion")
            || names.contains("suscripción")
        {
            return "ANNUAL".to_string();
        }

        if expires_at.is_some() {
            "ANNUAL".to_string()
        } else {
            "PRO".to_string()
        }
    }

    fn local_expiry_for_type(
        state: &LocalLicenseState,
        license_type: &str,
    ) -> Option<DateTime<Utc>> {
        let duration = match license_type {
            "TRIAL" => Some(TRIAL_DURATION_SEC),
            "FREE" => Some(FREE_DURATION_SEC),
            _ => None,
        }?;

        let start_date = state.activation_date.unwrap_or(state.install_date);
        Utc.timestamp_opt(start_date, 0)
            .single()
            .map(|start| start + Duration::seconds(duration))
    }

    fn effective_expiry(state: &LocalLicenseState, license_type: &str) -> Option<DateTime<Utc>> {
        if let Some(exp_str) = &state.expires_at {
            if let Some(exp_dt) = Self::parse_expiry(exp_str) {
                return Some(exp_dt);
            }
        }
        Self::local_expiry_for_type(state, license_type)
    }

    fn days_until(expiry: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
        let remaining = expiry.signed_duration_since(now).num_seconds();
        if remaining <= 0 {
            0
        } else {
            (remaining + 86_399) / 86_400
        }
    }

    fn is_license_current(
        state: &LocalLicenseState,
        license_type: &str,
        now: DateTime<Utc>,
    ) -> bool {
        if !state.is_pro_verified || state.license_key.is_none() {
            return false;
        }

        if matches!(license_type, "TRIAL" | "FREE") {
            let start_date = state.activation_date.unwrap_or(state.install_date);
            if now.timestamp() + 3600 < start_date {
                return false;
            }
        }

        if let Some(expiry) = Self::effective_expiry(state, license_type) {
            return now <= expiry;
        }

        true
    }

    fn validate_license_remote(key: String) -> Result<RemoteValidation, String> {
        thread::spawn(move || {
            let client = Client::builder()
                .timeout(std::time::Duration::from_secs(8))
                .build()
                .map_err(|e| format!("Error preparando validacion: {}", e))?;
            let params = [("license_key", key.as_str())];

            let res = client
                .post(LEMON_VALIDATE_URL)
                .form(&params)
                .send()
                .map_err(|e| format!("Error validando licencia: {}", e))?;

            let lemon_res: LemonValidateResponse = res
                .json()
                .map_err(|e| format!("Error interpretando validacion: {}", e))?;

            let expires_at = lemon_res
                .license_key
                .as_ref()
                .and_then(|license_key| license_key.expires_at.clone());
            let status = lemon_res
                .license_key
                .as_ref()
                .and_then(|license_key| license_key.status.clone())
                .unwrap_or_default()
                .to_lowercase();
            let type_from_remote = if lemon_res.meta.is_some() || expires_at.is_some() {
                Some(Self::detect_license_type(
                    expires_at.as_deref(),
                    lemon_res.meta.as_ref(),
                ))
            } else {
                None
            };

            let status_allows_access =
                !matches!(status.as_str(), "expired" | "disabled" | "inactive");

            Ok::<RemoteValidation, String>(RemoteValidation {
                is_valid: lemon_res.valid && status_allows_access,
                expires_at,
                license_type: type_from_remote,
            })
        })
        .join()
        .map_err(|_| "Error critico en hilo de validacion".to_string())?
    }

    fn refresh_remote_validation(&self, force: bool) {
        let now_ts = self.get_secure_now();
        let now_dt = Utc
            .timestamp_opt(now_ts, 0)
            .single()
            .unwrap_or_else(Utc::now);

        let (key, should_refresh) = {
            let guard = match self.state.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };

            // Revalidamos siempre que exista una clave, incluso si la licencia local
            // figura como expirada o no verificada. Así, si el usuario renueva o se
            // le extiende la licencia en LemonSqueezy, el cliente lo detecta en el
            // siguiente arranque o al vencer el intervalo de re-chequeo.
            let key = match guard.license_key.clone() {
                Some(key) => key,
                None => return,
            };

            let license_type = Self::normalized_type(guard.license_type.as_deref());
            let expiry_near = Self::effective_expiry(&guard, &license_type)
                .map(|expiry| {
                    expiry.signed_duration_since(now_dt).num_seconds()
                        <= RENEWAL_GRACE_CHECK_WINDOW_SEC
                })
                .unwrap_or(false);
            let stale = guard.last_check_date <= 0
                || now_ts.saturating_sub(guard.last_check_date) >= REMOTE_RECHECK_INTERVAL_SEC;

            // Si la licencia no está verificada (p. ej. expirada), reintentamos contra
            // el servidor respetando `stale` para no saturarlo. Si está verificada,
            // mantenemos el chequeo agresivo cerca del vencimiento y para las anuales.
            (
                key,
                force
                    || stale
                    || (guard.is_pro_verified && (expiry_near || license_type == "ANNUAL")),
            )
        };

        if !should_refresh {
            return;
        }

        if let Ok(remote) = Self::validate_license_remote(key) {
            let mut guard = match self.state.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };

            guard.is_pro_verified = remote.is_valid;
            guard.expires_at = remote.expires_at;
            if let Some(remote_type) = remote.license_type {
                guard.license_type = Some(remote_type);
            }
            guard.last_check_date = now_ts;
            self.save_state_to_disk(&guard);
        }
    }

    fn load_state_from_disk(&self) -> Option<LocalLicenseState> {
        let path = self.app_dir.join(LICENSE_FILE_NAME);
        if !path.exists() {
            return None;
        }

        if let Ok(encrypted_data) = fs::read(path) {
            if encrypted_data.len() < 12 {
                return None;
            }

            let (nonce_bytes, ciphertext) = encrypted_data.split_at(12);
            let nonce = Nonce::from_slice(nonce_bytes);
            let cipher = Aes256Gcm::new(&Self::get_encryption_key());

            if let Ok(plaintext) = cipher.decrypt(nonce, ciphertext) {
                if let Ok(json_str) = String::from_utf8(plaintext) {
                    if let Ok(state) = serde_json::from_str::<LocalLicenseState>(&json_str) {
                        return Some(state);
                    }
                }
            }
        }
        None
    }

    fn save_state_to_disk(&self, state: &LocalLicenseState) {
        if !self.app_dir.exists() {
            let _ = fs::create_dir_all(&self.app_dir);
        }

        // 1. Save to Main Disk File (Encrypted)
        let path = self.app_dir.join(LICENSE_FILE_NAME);
        let json_str = serde_json::to_string(state).unwrap();

        let cipher = Aes256Gcm::new(&Self::get_encryption_key());
        let mut rng = rand::thread_rng();
        let mut nonce_bytes = [0u8; 12];
        rng.fill(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        if let Ok(ciphertext) = cipher.encrypt(nonce, json_str.as_bytes()) {
            let mut final_data = nonce_bytes.to_vec();
            final_data.extend(ciphertext);
            let _ = fs::write(path, final_data);
        }

        // 2. Save to Shadow File (Encrypted)
        self.save_to_shadow(state);

        // 3. Save to Registry (Flags)
        self.save_to_registry(state);

        // 4. Save to ADS (Hidden Stream)
        self.save_to_ads(state);
    }

    // --- PERSISTENCE HELPERS ---

    #[cfg(target_os = "windows")]
    fn get_ads_path(&self) -> Option<(PathBuf, String)> {
        // Target: %LOCALAPPDATA%\ZenithStorage\sys_config_cache.dat:secure_license_stream
        if let Ok(local_app_data) = env::var("LOCALAPPDATA") {
            let p = PathBuf::from(local_app_data).join("ZenithStorage");
            if !p.exists() {
                let _ = fs::create_dir_all(&p);
            }
            let base_file = p.join(ADS_FILE_NAME);

            // Format: path/to/file:stream
            let ads_path_str = format!("{}:{}", base_file.to_string_lossy(), ADS_STREAM_NAME);

            return Some((base_file, ads_path_str));
        }
        None
    }

    #[cfg(not(target_os = "windows"))]
    fn get_ads_path(&self) -> Option<(PathBuf, String)> {
        None
    }

    fn load_from_ads(&self) -> Option<LocalLicenseState> {
        if let Some((base_file, ads_path_str)) = self.get_ads_path() {
            // ADS requires the base file to exist
            if base_file.exists() {
                // Try to read the stream directly
                if let Ok(encrypted_data) = fs::read(&ads_path_str) {
                    if encrypted_data.len() < 12 {
                        return None;
                    }
                    let (nonce_bytes, ciphertext) = encrypted_data.split_at(12);
                    let nonce = Nonce::from_slice(nonce_bytes);
                    let cipher = Aes256Gcm::new(&Self::get_encryption_key());
                    if let Ok(plaintext) = cipher.decrypt(nonce, ciphertext) {
                        if let Ok(json_str) = String::from_utf8(plaintext) {
                            if let Ok(state) = serde_json::from_str::<LocalLicenseState>(&json_str)
                            {
                                return Some(state);
                            }
                        }
                    }
                }
            }
        }
        None
    }

    fn save_to_ads(&self, state: &LocalLicenseState) {
        if let Some((base_file, ads_path_str)) = self.get_ads_path() {
            // 1. Ensure base file exists (can be empty or dummy content)
            if !base_file.exists() {
                let _ = fs::write(&base_file, b"System Configuration Cache - Do Not Delete");
                // Hide the folder/file minimally?
                #[cfg(target_os = "windows")]
                {
                    use std::process::Command;
                    let _ = Command::new("attrib")
                        .args(&["+h", base_file.to_str().unwrap()])
                        .output();
                }
            }

            // 2. Write to ADS
            let json_str = serde_json::to_string(state).unwrap();
            let cipher = Aes256Gcm::new(&Self::get_encryption_key());
            let mut rng = rand::thread_rng();
            let mut nonce_bytes = [0u8; 12];
            rng.fill(&mut nonce_bytes);
            let nonce = Nonce::from_slice(&nonce_bytes);

            if let Ok(ciphertext) = cipher.encrypt(nonce, json_str.as_bytes()) {
                let mut final_data = nonce_bytes.to_vec();
                final_data.extend(ciphertext);
                // Writing to "file:stream"
                let _ = fs::write(ads_path_str, final_data);
            }
        }
    }

    fn get_shadow_path(&self) -> Option<PathBuf> {
        if let Ok(home) = env::var("USERPROFILE").or_else(|_| env::var("HOME")) {
            let p = PathBuf::from(home).join(SHADOW_DIR_NAME);
            if !p.exists() {
                let _ = fs::create_dir_all(&p);
            }
            // Make hidden on Windows
            #[cfg(target_os = "windows")]
            {
                use std::process::Command;
                let _ = Command::new("attrib")
                    .args(&["+h", p.to_str().unwrap()])
                    .output();
            }
            return Some(p.join(SHADOW_FILE_NAME));
        }
        None
    }

    fn load_from_shadow(&self) -> Option<LocalLicenseState> {
        if let Some(path) = self.get_shadow_path() {
            if path.exists() {
                if let Ok(encrypted_data) = fs::read(path) {
                    // Decrypt logic duplicated for safety
                    if encrypted_data.len() < 12 {
                        return None;
                    }
                    let (nonce_bytes, ciphertext) = encrypted_data.split_at(12);
                    let nonce = Nonce::from_slice(nonce_bytes);
                    let cipher = Aes256Gcm::new(&Self::get_encryption_key());
                    if let Ok(plaintext) = cipher.decrypt(nonce, ciphertext) {
                        if let Ok(json_str) = String::from_utf8(plaintext) {
                            if let Ok(state) = serde_json::from_str::<LocalLicenseState>(&json_str)
                            {
                                return Some(state);
                            }
                        }
                    }
                }
            }
        }
        None
    }

    fn save_to_shadow(&self, state: &LocalLicenseState) {
        if let Some(path) = self.get_shadow_path() {
            let json_str = serde_json::to_string(state).unwrap();
            let cipher = Aes256Gcm::new(&Self::get_encryption_key());
            let mut rng = rand::thread_rng();
            let mut nonce_bytes = [0u8; 12];
            rng.fill(&mut nonce_bytes);
            let nonce = Nonce::from_slice(&nonce_bytes);

            if let Ok(ciphertext) = cipher.encrypt(nonce, json_str.as_bytes()) {
                let mut final_data = nonce_bytes.to_vec();
                final_data.extend(ciphertext);
                let _ = fs::write(path, final_data);
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn load_from_registry(&self) -> Option<LocalLicenseState> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok(key) = hkcu.open_subkey(REGISTRY_PATH) {
            let has_used_trial: u32 = key.get_value("HasUsedTrial").unwrap_or(0);
            let install_date: u64 = key.get_value("InstallDate").unwrap_or(0);

            // Reconstruct minimal state focused on restrictions
            return Some(LocalLicenseState {
                install_date: install_date as i64,
                last_check_date: 0,
                license_key: None,
                instance_id: None,
                is_pro_verified: false,
                has_used_trial: has_used_trial > 0,
                license_type: None, // We don't store type in registry to avoid spoofing PRO, only restrictions
                activation_date: None,
                signature: "registry".to_string(),
                expires_at: None,
            });
        }
        None
    }

    #[cfg(not(target_os = "windows"))]
    fn load_from_registry(&self) -> Option<LocalLicenseState> {
        None
    }

    #[cfg(target_os = "windows")]
    fn save_to_registry(&self, state: &LocalLicenseState) {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok((key, _)) = hkcu.create_subkey(REGISTRY_PATH) {
            let _ = key.set_value(
                "HasUsedTrial",
                &(if state.has_used_trial { 1u32 } else { 0u32 }),
            );
            if state.install_date > 0 {
                let _ = key.set_value("InstallDate", &(state.install_date as u64));
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn save_to_registry(&self, _state: &LocalLicenseState) {}

    fn initialize(&self) {
        let now = self.get_secure_now();

        // 1. Load from all sources
        let disk_state = self.load_state_from_disk();
        let shadow_state = self.load_from_shadow();
        let reg_state = self.load_from_registry();
        let ads_state = self.load_from_ads();

        let mut final_state = LocalLicenseState {
            install_date: now,
            last_check_date: now,
            license_key: None,
            instance_id: None,
            is_pro_verified: false,
            has_used_trial: false,
            license_type: None,
            activation_date: None,
            signature: "init".to_string(),
            expires_at: None,
        };

        // 2. CONSOLIDATION LOGIC (Restrictive Merge)

        // Base on Disk (Most feature-rich)
        if let Some(ds) = disk_state {
            final_state = ds;
        } else if let Some(ss) = shadow_state.clone() {
            // Restore from shadow if disk missing
            final_state = ss;
        } else if let Some(adss) = ads_state.clone() {
            // Restore from ADS if disk & shadow missing
            final_state = adss;
        }

        // Apply restrictions from Shadow (if disk was reset but shadow exists)
        if let Some(ss) = shadow_state {
            if ss.has_used_trial {
                final_state.has_used_trial = true;
            }
            if final_state.install_date == 0
                || (ss.install_date > 0 && ss.install_date < final_state.install_date)
            {
                final_state.install_date = ss.install_date;
            }
        }

        // Apply restrictions from ADS
        if let Some(adss) = ads_state {
            if adss.has_used_trial {
                final_state.has_used_trial = true;
            }
            if final_state.install_date == 0
                || (adss.install_date > 0 && adss.install_date < final_state.install_date)
            {
                final_state.install_date = adss.install_date;
            }
        }

        // Apply restrictions from Registry
        if let Some(rs) = reg_state {
            if rs.has_used_trial {
                final_state.has_used_trial = true;
            }
            if final_state.install_date == 0
                || (rs.install_date > 0 && rs.install_date < final_state.install_date)
            {
                final_state.install_date = rs.install_date;
            }
        }

        // Update checks
        if final_state.last_check_date > now + 3600 {
            // Anti-rollback warn?
        } else {
            final_state.last_check_date = now;
        }

        let mut state_guard = match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *state_guard = final_state.clone();

        if state_guard.license_key.is_some() {
            // Verificamos contra el servidor aunque la licencia local figure como
            // expirada/no verificada: así detectamos renovaciones o extensiones
            // hechas en LemonSqueezy sin que el usuario tenga que reinstalar.
            drop(state_guard);
            self.verify_pro_license_silent();
        } else {
            // Save consolidated state to all
            self.save_state_to_disk(&state_guard); // This function needs to be updated to call others
        }
    }

    fn verify_pro_license_silent(&self) {
        self.refresh_remote_validation(true);
    }

    // ACTUALIZADO: Devuelve TRUE solo si la licencia esta VERIFICADA y ACTIVA
    /// Bypass exclusivo de builds locales (benchmark/desarrollo): requiere la
    /// feature de compilación `dev-license` (los binarios distribuidos no
    /// contienen este código) Y la variable ZAS_DEV_LICENSE=1 en runtime.
    #[cfg(feature = "dev-license")]
    fn dev_license_bypass() -> bool {
        std::env::var("ZAS_DEV_LICENSE").ok().as_deref() == Some("1")
    }

    pub fn is_pro(&self) -> bool {
        #[cfg(feature = "dev-license")]
        if Self::dev_license_bypass() {
            return true;
        }
        let state_snapshot = {
            let guard = match self.state.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.clone()
        };

        let license_type = Self::normalized_type(state_snapshot.license_type.as_deref());
        let now_ts = self.get_secure_now();
        let now_dt = Utc
            .timestamp_opt(now_ts, 0)
            .single()
            .unwrap_or_else(Utc::now);

        Self::has_full_access_type(&license_type)
            && Self::is_license_current(&state_snapshot, &license_type, now_dt)
    }

    pub fn get_status(&self) -> AppStatus {
        #[cfg(feature = "dev-license")]
        if Self::dev_license_bypass() {
            return AppStatus {
                status: "PRO".to_string(),
                days_remaining: 3650,
                is_pro: true,
                message: "Licencia de desarrollo local (dev-license).".to_string(),
                license_key: Some("DEV-LOCAL".to_string()),
                license_type: "PRO".to_string(),
                registered_at: None,
                expires_at: None,
                last_checked_at: None,
                renewal_status: None,
            };
        }
        self.refresh_remote_validation(false);

        let guard = match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        let license_type = Self::normalized_type(guard.license_type.as_deref());
        let license_key = guard
            .license_key
            .as_ref()
            .map(|key| Self::mask_license_key(key));
        let registered_at = guard.activation_date.and_then(Self::format_timestamp);
        let last_checked_at = Self::format_timestamp(guard.last_check_date);
        let expires_at = guard
            .expires_at
            .as_ref()
            .map(|exp| Self::format_expiry(exp));
        let base_type = Self::display_license_type(&license_type);

        // Estado Bloqueado por Defecto (Sin clave)
        if guard.license_key.is_none() {
            return AppStatus {
                status: "LOCKED".to_string(),
                days_remaining: 0,
                is_pro: false,
                message: "Se requiere activación.".to_string(),
                license_key,
                license_type: base_type,
                registered_at,
                expires_at,
                last_checked_at,
                renewal_status: None,
            };
        }

        if !guard.is_pro_verified {
            let renewal_status = if license_type == "ANNUAL" {
                Some("Pago no renovado o licencia anual no vigente.".to_string())
            } else {
                None
            };

            return AppStatus {
                status: "EXPIRED".to_string(),
                days_remaining: 0,
                is_pro: false,
                message: match license_type.as_str() {
                    "TRIAL" => "La prueba gratuita no está vigente.".to_string(),
                    "FREE" => "La licencia gratuita no está vigente.".to_string(),
                    "ANNUAL" => "La licencia anual no está vigente o no fue renovada.".to_string(),
                    _ => "La licencia no está vigente.".to_string(),
                },
                license_key,
                license_type: base_type,
                registered_at,
                expires_at,
                last_checked_at,
                renewal_status,
            };
        }

        let now_ts = self.get_secure_now();
        let now_utc = Utc
            .timestamp_opt(now_ts, 0)
            .single()
            .unwrap_or_else(Utc::now);
        let expiry = Self::effective_expiry(&guard, &license_type);
        let days_remaining = expiry
            .map(|exp| Self::days_until(exp, now_utc))
            .unwrap_or(9999);
        let is_current = Self::is_license_current(&guard, &license_type, now_utc);
        let is_full_access = Self::has_full_access_type(&license_type) && is_current;

        if !is_current {
            let renewal_status = if license_type == "ANNUAL" {
                Some("Pago no renovado o licencia anual expirada.".to_string())
            } else {
                None
            };

            return AppStatus {
                status: "EXPIRED".to_string(),
                days_remaining: 0,
                is_pro: false,
                message: match license_type.as_str() {
                    "TRIAL" => "La prueba gratuita ha finalizado.".to_string(),
                    "FREE" => "La licencia gratuita ha expirado.".to_string(),
                    "ANNUAL" => "La licencia anual ha expirado o no fue renovada.".to_string(),
                    _ => "Tu licencia ha expirado.".to_string(),
                },
                license_key,
                license_type: base_type,
                registered_at,
                expires_at: expiry
                    .map(|exp| exp.format("%Y-%m-%d %H:%M UTC").to_string())
                    .or(expires_at),
                last_checked_at,
                renewal_status,
            };
        }

        let formatted_expiry = expiry
            .map(|exp| exp.format("%Y-%m-%d %H:%M UTC").to_string())
            .or(expires_at);
        let renewal_status = if license_type == "ANNUAL" {
            Some(
                last_checked_at
                    .as_ref()
                    .map(|date| format!("Pago y renovación verificados el {}.", date))
                    .unwrap_or_else(|| "Pago y renovación verificados.".to_string()),
            )
        } else {
            None
        };

        match license_type.as_str() {
            "TRIAL" => AppStatus {
                status: "TRIAL".to_string(),
                days_remaining,
                is_pro: is_full_access,
                message: format!("Prueba gratuita activa. Quedan {} días.", days_remaining),
                license_key,
                license_type: base_type,
                registered_at,
                expires_at: formatted_expiry,
                last_checked_at,
                renewal_status,
            },
            "FREE" => AppStatus {
                status: "FREE".to_string(),
                days_remaining,
                is_pro: false,
                message: format!(
                    "Licencia gratuita activa. Quedan {} días con funciones limitadas.",
                    days_remaining
                ),
                license_key,
                license_type: base_type,
                registered_at,
                expires_at: formatted_expiry,
                last_checked_at,
                renewal_status,
            },
            "ANNUAL" => AppStatus {
                status: "ANNUAL".to_string(),
                days_remaining,
                is_pro: is_full_access,
                message: formatted_expiry
                    .as_ref()
                    .map(|exp| format!("Licencia anual válida hasta {}.", exp))
                    .unwrap_or_else(|| "Licencia anual activa.".to_string()),
                license_key,
                license_type: base_type,
                registered_at,
                expires_at: formatted_expiry,
                last_checked_at,
                renewal_status,
            },
            _ => AppStatus {
                status: "PRO".to_string(),
                days_remaining: 9999,
                is_pro: is_full_access,
                message: "Licencia PRO activa.".to_string(),
                license_key,
                license_type: base_type,
                registered_at,
                expires_at: formatted_expiry,
                last_checked_at,
                renewal_status,
            },
        }
    }

    pub fn check_access(&self) -> Result<(), String> {
        if !self.is_pro() {
            return Err(
                "Acceso denegado. Se requiere una prueba vigente o licencia anual activa."
                    .to_string(),
            );
        }
        Ok(())
    }

    pub fn activate_license(&self, key: &str, device_name: &str) -> Result<String, String> {
        let key = key.to_string();
        let device_name = device_name.to_string();
        let key_for_thread = key.clone();

        let result = thread::spawn(move || {
            let client = Client::new();
            let machine_id = machine_uid::get().unwrap_or("unknown".into());
            let full_name = format!("{} - {}", device_name, machine_id);

            let params = [
                ("license_key", key_for_thread.as_str()),
                ("instance_name", full_name.as_str()),
            ];

            let res = client
                .post(LEMON_API_URL)
                .form(&params)
                .send()
                .map_err(|e| format!("Error de conexion: {}", e))?;

            let lemon_res: LemonResponse = res
                .json()
                .map_err(|e| format!("Error interpretando respuesta: {}", e))?;

            Ok::<LemonResponse, String>(lemon_res)
        })
        .join();

        let lemon_res = match result {
            Ok(Ok(res)) => res,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err("Error critico en hilo de activacion".to_string()),
        };

        if lemon_res.activated {
            let mut guard = match self.state.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };

            let expires_at_val = lemon_res
                .license_key
                .as_ref()
                .and_then(|key_info| key_info.expires_at.clone());
            let detected_type =
                Self::detect_license_type(expires_at_val.as_deref(), lemon_res.meta.as_ref());
            let now = self.get_secure_now();
            let now_dt = Utc.timestamp_opt(now, 0).single().unwrap_or_else(Utc::now);

            if let Some(exp_str) = &expires_at_val {
                if let Some(exp_dt) = Self::parse_expiry(exp_str) {
                    if now_dt > exp_dt {
                        return Err("La licencia esta expirada o no fue renovada.".to_string());
                    }
                }
            }

            // Validacion de Abuso de Trial
            if detected_type == "TRIAL" {
                if guard.has_used_trial {
                    // BLOQUEO ESTRICTO: Si ya se uso un trial en esta maquina, no permitir otro.
                    return Err("Esta computadora ya ha utilizado una licencia de prueba anteriormente. Para continuar, adquiere una licencia anual.".to_string());
                }
                guard.has_used_trial = true;
            }

            guard.license_key = Some(
                lemon_res
                    .license_key
                    .as_ref()
                    .and_then(|k| k.key.clone())
                    .unwrap_or(key),
            );
            guard.instance_id = lemon_res.instance.map(|i| i.id);
            guard.is_pro_verified = true;
            guard.last_check_date = now;
            guard.activation_date = Some(now);

            guard.license_type = Some(detected_type.clone());
            guard.expires_at = expires_at_val;

            self.save_state_to_disk(&guard);

            if detected_type == "TRIAL" {
                return Ok(
                    "Evaluación activada. El tiempo se validará con el servidor.".to_string(),
                );
            } else if detected_type == "FREE" {
                return Ok("Licencia Gratuita activada.".to_string());
            } else if detected_type == "ANNUAL" {
                return Ok(
                    "Licencia anual activada. Gracias por renovar Zenith Astro Stacker."
                        .to_string(),
                );
            } else {
                return Ok("Licencia PRO activada. Gracias.".to_string());
            }
        }

        Err(lemon_res
            .error
            .unwrap_or_else(|| "Clave invalida o expirada.".to_string()))
    }

    pub fn deactivate(&self) -> Result<(), String> {
        let mut guard = match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        // REMOTE DEACTIVATION (Best Effort)
        if let (Some(key), Some(instance_id)) = (&guard.license_key, &guard.instance_id) {
            let k_clone = key.clone();
            let i_clone = instance_id.clone();

            // Spawn thread to avoid blocking main thread, though we want to know if it failed?
            // For deactivation, we usually prioritize clearing local state so user can switch.
            // We will do a blocking call here or spawn and ignore error, but blocking is safer to ensure it registered.
            // Given this is a "Deactivate" action, user expects it to be done.
            thread::spawn(move || {
                let client = Client::new();
                let params = [
                    ("license_key", k_clone.as_str()),
                    ("instance_id", i_clone.as_str()),
                ];

                let _ = client
                    .post("https://api.lemonsqueezy.com/v1/licenses/deactivate")
                    .form(&params)
                    .send(); // We ignore the result, just try to deactivate
            })
            .join()
            .unwrap_or(());
        }

        guard.license_key = None;
        guard.instance_id = None;
        guard.is_pro_verified = false;
        guard.license_type = None;
        guard.activation_date = None;
        guard.expires_at = None;

        self.save_state_to_disk(&guard);
        Ok(())
    }

    pub fn reset_license_internal(&self) {
        let mut guard = match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.has_used_trial = false;
        guard.license_key = None;
        guard.instance_id = None;
        guard.is_pro_verified = false;
        guard.license_type = None;
        guard.activation_date = None;
        guard.expires_at = None;
        self.save_state_to_disk(&guard);
        // ADS y otras persistencias se actualizan automáticamente dentro de save_state_to_disk con el estado limpio.
    }
}
