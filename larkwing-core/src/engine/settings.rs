//! 设置读写:白名单 / 逐键写入校验 / 下载认证 / 皮肤 / 应用密钥对。
//!
//! 新增 settings 键 = 两边各加一行(§6.8);**app 级键是三处**:前端 DEFAULTS +
//! `APP_SETTING_KEYS`(读过桥)+ `set_setting` 的 match 臂(写入校验,兜底一律拒)。

use super::*;

/// app 级设置里允许过桥给前端的 key —— 含钥匙的(llm.api_key / llm.providers)永不在列。
const APP_SETTING_KEYS: &[&str] = &[
    "llm.strategy",
    "llm.thinking",
    "voice.input_device",
    "voice.wake.enabled",
    // (voice.wake.keywords 已退役:唤醒词 = 名字派生,2026-07-10;存量行成死数据无害)
    "voice.wake.sensitivity",
    "voice.asr.model",
    "voice.tts_backend",
    // 采集源(app 级,层1 AEC 采集端):browser(默认,2026-07-06 转正:前端 getUserMedia
    // 消完回声推流)| cpal(回落/兼容)。§6.8 两边各加一行。
    "voice.capture.source",
    // 浏览器采集的麦克风(enumerateDevices 的 deviceId;空 = 系统默认)。与 cpal 的
    // voice.input_device(人类可读名)分键 —— 两套命名空间,绝不混写。
    "voice.input_device_web",
    "weather.qweather.host",
    "weather.qweather.project_id",
    "weather.qweather.credential_id",
    "net.proxy",
    "net.proxy_enabled",
    "memory.auto_consolidate",
    "audio.leveling",
    "audio.night_mode",
    "audio.night_start",
    "audio.night_end",
    // 主动关怀总开关(app 级,PLAN ★主动关怀里程碑):0/1,默认开。§6.8 两边各加一行。
    "care.enabled",
    // 自动备份目标目录(autobackup.rs):非空 = 开、清空 = 关(选了目录即开,无独立开关键)。
    "backup.auto.dir",
];

/// 语音的用户级设置(PLAN §11 逐键放行,不开 voice.* 通配——同前缀跨两个 scope)。
const VOICE_USER_KEYS: &[&str] =
    &["voice.speaker", "voice.auto_speak", "voice.rate", "voice.patience", "voice.volume"];

#[derive(Debug, Clone, Serialize)]
pub struct SettingEntry {
    /// "app" | "user"
    pub scope: String,
    pub key: String,
    pub value: String,
}

// ---------- 首屏快照 ----------

impl Engine {
    pub fn set_skin(&self, skin_id: &str) -> Result<(), AppError> {
        let user = self.store.users.ensure_default_user()?;
        self.store.users.set_skin(user.id, skin_id)?;
        Ok(())
    }

    /// 当前用户皮肤。给无 boot 快照的窗口(悬浮窗)拉取初值用;主窗已从 boot 拿到。
    pub fn skin(&self) -> Result<String, AppError> {
        Ok(self.store.users.ensure_default_user()?.skin_id)
    }

    /// 确保**全局** Ed25519 身份密钥对存在(没有就生成并落库),返回公钥 PEM 给前端展示/复制。
    /// 幂等:已有直接回存量公钥。私钥(`crypto.ed25519.private_key`)是秘密、永不过桥。所有走
    /// Ed25519-JWT 的服务(和风是首个消费者)共用这一把 ——「整个程序对外一对」。
    pub fn ensure_app_keypair(&self) -> Result<String, AppError> {
        use crate::crypto::{generate_keypair, KEY_ED25519_PRIVATE, KEY_ED25519_PUBLIC};
        if let Some(pubkey) = self.store.settings.get(None, KEY_ED25519_PUBLIC)? {
            if !pubkey.trim().is_empty() {
                // ⚠️ **只看公钥就早退是不够的(2026-08-22 修)**:公钥住 settings(在库里,
                // **备份带得走**),私钥住系统密钥串(**备份带不走**,§6.2 已记档)。于是
                // 「换台机器从备份恢复」之后,库里有公钥、密钥串里没有私钥 —— 这里直接早退,
                // 密钥对就**永久错配**:签 JWT 时拿不到私钥,和风天气这类服务一路静默降级,
                // 用户只觉得「天气怎么不准了」,日志里连句话都没有(§3.5)。
                // 私钥真的在,才算这对是完好的;不在就往下走、重新生成一对。
                let has_private = crate::secrets::get(&self.store.settings, KEY_ED25519_PRIVATE)
                    .is_some_and(|p| !p.trim().is_empty());
                if has_private {
                    return Ok(pubkey);
                }
                tracing::warn!(
                    "应用密钥对不完整(库里有公钥、系统密钥串里没有私钥)——多半是把备份恢复到了\
                     另一台机器。重新生成一对;之前把公钥登记到外部服务(如和风天气)的话,\
                     要拿设置·服务页上的新公钥再登记一次。"
                );
            }
        }
        let (private_pem, public_pem) = generate_keypair().map_err(|e| AppError {
            kind: ErrorKind::Internal,
            message: format!("生成应用密钥失败:{e}"),
        })?;
        // 私钥进 keyring(秘密、永不过桥);公钥留 settings(非秘密,给用户复制)
        crate::secrets::set(&self.store.settings, KEY_ED25519_PRIVATE, &private_pem)
            .map_err(AppError::internal)?;
        self.store.settings.set(None, KEY_ED25519_PUBLIC, &public_pem)?;
        Ok(public_pem)
    }

    /// 设置页快照:app 级只暴露白名单内的 key(llm.api_key / llm.providers
    /// 这类含钥匙的永不过桥),用户级只暴露 ui.* 前缀。
    pub fn list_settings(&self) -> Result<Vec<SettingEntry>, AppError> {
        let user = self.store.users.ensure_default_user()?;
        let mut out = Vec::new();
        for (key, value) in self.store.settings.list(None)? {
            if APP_SETTING_KEYS.contains(&key.as_str()) {
                out.push(SettingEntry { scope: "app".into(), key, value });
            }
        }
        for (key, value) in self.store.settings.list(Some(user.id))? {
            if key.starts_with("ui.")
                || key == "persona.style"
                || VOICE_USER_KEYS.contains(&key.as_str())
            {
                out.push(SettingEntry { scope: "user".into(), key, value });
            }
        }
        Ok(out)
    }

    /// 写设置:key 决定归属与合法值,白名单之外一律拒绝(PLAN:不开无类型后门)。
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), AppError> {
        let invalid = |msg: &str| AppError { kind: ErrorKind::Internal, message: msg.into() };
        if value.chars().count() > 200 {
            return Err(invalid("设置值过长"));
        }
        match key {
            "llm.strategy" => {
                if !["thrifty", "balanced", "smart_first"].contains(&value) {
                    return Err(invalid("未知的用脑策略"));
                }
                self.store.settings.set(None, key, value)?;
                self.reload_providers() // 策略变了 = 候选顺序变了
            }
            "llm.thinking" => {
                if !["off", "light", "medium", "heavy"].contains(&value) {
                    return Err(invalid("未知的反应模式档位"));
                }
                self.store.settings.set(None, key, value)?;
                Ok(()) // 每回合取值,无需重建
            }
            // 一句话性格设定(用户级):进稳定前缀的人格覆盖层,改动即生效(下一回合重装配)
            "persona.style" => {
                if value.chars().count() > 500 {
                    return Err(invalid("性格设定最多 500 字"));
                }
                let user = self.store.users.ensure_default_user()?;
                self.store.settings.set(Some(user.id), key, value)?;
                Ok(())
            }
            k if k.starts_with("ui.") => {
                let user = self.store.users.ensure_default_user()?;
                self.store.settings.set(Some(user.id), k, value)?;
                Ok(())
            }
            // 语音(PLAN §11):有枚举的逐键校验;user 级跟人走,app 级是机器属性
            "voice.patience" => {
                if !["snappy", "standard", "relaxed"].contains(&value) {
                    return Err(invalid("未知的耐心档位"));
                }
                let user = self.store.users.ensure_default_user()?;
                self.store.settings.set(Some(user.id), key, value)?;
                Ok(())
            }
            "voice.auto_speak" => {
                if !["follow", "always", "off"].contains(&value) {
                    return Err(invalid("未知的自动朗读档位"));
                }
                let user = self.store.users.ensure_default_user()?;
                self.store.settings.set(Some(user.id), key, value)?;
                Ok(())
            }
            "voice.speaker" | "voice.rate" | "voice.volume" => {
                let user = self.store.users.ensure_default_user()?;
                self.store.settings.set(Some(user.id), key, value)?;
                Ok(())
            }
            "voice.input_device" | "voice.input_device_web" => {
                self.store.settings.set(None, key, value)?;
                Ok(())
            }
            "voice.capture.source" => {
                // auto(默认,2026-08-12)= 按「默认输出是不是耳机」现解析,voice::capture_route
                if !["auto", "cpal", "browser"].contains(&value) {
                    return Err(invalid("未知的采集源"));
                }
                self.store.settings.set(None, key, value)?;
                Ok(())
            }
            // 唤醒词无独立设置:= 名字派生(ui.pet_name → voice::wake_keywords,2026-07-10
            // 「起什么名字就怎么唤醒」)。voice.wake.enabled 也不走这里——开关 = voice_wake_set
            // 一体化入口(写库 + 起停),绕过会出现"库说开着、循环没在跑"的分叉。
            // 唤醒灵敏度(app 级,机器属性):0~100 整数 → wake_threshold 映射成 KWS 阈值。
            // 漏了这条 → 写被白名单拒 → 前端乐观写回滚,滑块"一闪一闪"且从不落库(灵敏度其实没生效)。
            // 开着唤醒时前端 saveSensitivity 会重启循环让新阈值生效。
            "voice.wake.sensitivity" => match value.parse::<u32>() {
                Ok(n) if n <= 100 => {
                    self.store.settings.set(None, key, value)?;
                    Ok(())
                }
                _ => Err(invalid("唤醒灵敏度需为 0~100 的整数")),
            },
            "voice.tts_backend" => {
                if !["online", "offline"].contains(&value) {
                    return Err(invalid("未知的语音合成档"));
                }
                self.store.settings.set(None, key, value)?;
                Ok(())
            }
            // 中文 ASR 模型档(app 级,机器属性,2026-06 用户要求放出来选,AGENT §7.5):
            // sense-voice(快,默认)/ firered-ctc(最准·口音/孩子,~740MB)/ funasr-nano(远场
            // 高噪/方言,~264MB)/ paraformer(老牌备胎,~230MB)——2026-08-28 扩 4 档,值与
            // models.rs::AsrModel::from_setting 同源。模型用时下载;开着唤醒时前端会重启循环让
            // 新模型生效(同 sensitivity)。漏了这条 → 写被白名单拒 → 前端乐观写回滚。
            "voice.asr.model" => {
                if !["sense-voice", "firered-ctc", "funasr-nano", "paraformer"].contains(&value) {
                    return Err(invalid("未知的识别模型档"));
                }
                self.store.settings.set(None, key, value)?;
                Ok(())
            }
            // 和风 JWT 接入(app 级,非秘密):项目 ID(JWT 的 sub)/凭据 ID(JWT 的 kid),空串 = 清空。
            // 三件套(含 host)齐 + 全局私钥已生成 → 下回合工具现读即切和风源,否则回落 Open-Meteo。
            "weather.qweather.project_id" | "weather.qweather.credential_id" => {
                self.store.settings.set(None, key, value.trim())?;
                Ok(())
            }
            // 和风专属 API Host(app 级,非秘密):空 = 不走和风(老公共域名已停服,无免 host 兜底)。
            // 控制台给的是**裸域名**(xxx.qweatherapi.com),用户多半不带 scheme —— 缺 scheme 自动补
            // https://(和风一律 https),绝不因「没写 http」就拒,否则乐观写回滚,用户「填不进去」。
            "weather.qweather.host" => {
                let v = value.trim();
                let v = if v.is_empty() || v.starts_with("http://") || v.starts_with("https://") {
                    v.to_string()
                } else {
                    format!("https://{v}")
                };
                self.store.settings.set(None, key, &v)?;
                Ok(())
            }
            // 全局代理地址(app 级,PLAN §代理):单独保存、始终保留(用不用看总开关 net.proxy_enabled);
            // 取值 http(s)/socks5(h) 或 ${ENV}(空也允许 = 占位,开关开时回落系统 env);写库后刷新全局 net。
            "net.proxy" => {
                let v = value.trim();
                let ok = v.is_empty()
                    || v.contains("${")
                    || ["http://", "https://", "socks5://", "socks5h://"]
                        .iter()
                        .any(|p| v.starts_with(p));
                if !ok {
                    return Err(invalid("代理地址要以 http(s):// 或 socks5(h):// 开头(或留空)"));
                }
                self.store.settings.set(None, key, v)?;
                crate::net::set_proxy(Self::resolve_proxy(&self.store));
                Ok(())
            }
            // 全局代理总开关(app 级):0/1。关 = 一律直连(地址照存不丢);开 = 用上面的地址。
            // 写库后立即刷新全局 net(现读即生效,无需重启)。地址与开关分家 = 关掉不丢地址。
            "net.proxy_enabled" => {
                if !["0", "1"].contains(&value) {
                    return Err(invalid("开关需为 0 或 1"));
                }
                self.store.settings.set(None, key, value)?;
                crate::net::set_proxy(Self::resolve_proxy(&self.store));
                Ok(())
            }
            // 记忆自动提炼总开关(app 级,PLAN §13 Phase 3):0/1,默认开(缺省 = 开,见 spawn_consolidate)。
            // 关 = 不再后台蒸馏(手动 consolidate_conversation 入口不受影响);现读即生效,无需重启。
            "memory.auto_consolidate" => {
                if !["0", "1"].contains(&value) {
                    return Err(invalid("开关需为 0 或 1"));
                }
                self.store.settings.set(None, key, value)?;
                Ok(())
            }
            // 主动关怀总开关(app 级,PLAN ★主动关怀里程碑):0/1,默认开(缺省 = 开,见 float_idle 关怀候选)。
            // 关 = 悬浮窗不投关怀候选、in-chat 场景续接 chips 也收起;现读即生效,无需重启。
            "care.enabled" => {
                if !["0", "1"].contains(&value) {
                    return Err(invalid("开关需为 0 或 1"));
                }
                self.store.settings.set(None, key, value)?;
                Ok(())
            }
            // 响度均衡 / 夜间模式(app 级,机器属性;客户端 Web Audio 消费,见 useAudioGraph.ts):
            // leveling 0/1 = 总开关(关 = 不接管播放、原样输出,兼作 Web Audio 兜底关);night_mode
            // off/on/auto;night_start/end = "HH:MM"(auto 生效时段,可跨零点)。现读即生效,无需重启。
            "audio.leveling" => {
                if !["0", "1"].contains(&value) {
                    return Err(invalid("开关需为 0 或 1"));
                }
                self.store.settings.set(None, key, value)?;
                Ok(())
            }
            "audio.night_mode" => {
                if !["off", "on", "auto"].contains(&value) {
                    return Err(invalid("未知的夜间模式档位"));
                }
                self.store.settings.set(None, key, value)?;
                Ok(())
            }
            "audio.night_start" | "audio.night_end" => {
                let ok = {
                    let mut it = value.splitn(2, ':');
                    match (
                        it.next().and_then(|h| h.parse::<u32>().ok()),
                        it.next().and_then(|m| m.parse::<u32>().ok()),
                    ) {
                        (Some(h), Some(m)) => h < 24 && m < 60,
                        _ => false,
                    }
                };
                if !ok {
                    return Err(invalid("时间需为 HH:MM"));
                }
                self.store.settings.set(None, key, value)?;
                Ok(())
            }
            // 自动备份目标目录(app 级,autobackup.rs):非空 = 开(须绝对路径,来自原生
            // 目录选择器)、清空 = 关。水位线循环现读即生效,无需重启。
            "backup.auto.dir" => {
                let v = value.trim();
                if !v.is_empty() && !std::path::Path::new(v).is_absolute() {
                    return Err(invalid("备份目录需要绝对路径"));
                }
                self.store.settings.set(None, key, v)?;
                Ok(())
            }
            // 远程渠道配置(app 级,PLAN 远程渠道):enabled 校验 0/1;凭证/白名单原样写(trim)。
            // token/app_secret 等**不进 APP_SETTING_KEYS** → 写得进、读不回(钥匙永不过桥,§4)。
            // 改完由前端调 reload_channels 命令停旧起新(类比 provider 保存即重建)。
            k if k.starts_with("remote.") => {
                if k.ends_with(".enabled") && !["0", "1"].contains(&value) {
                    return Err(invalid("开关需为 0 或 1"));
                }
                // token/app_secret 等是秘密 → keyring(写得进读不回);开关/白名单非秘密走 settings
                if crate::secrets::is_secret(k) {
                    crate::secrets::set(&self.store.settings, k, value.trim())
                        .map_err(AppError::internal)?;
                } else {
                    self.store.settings.set(None, k, value.trim())?;
                }
                Ok(())
            }
            _ => Err(invalid("不在设置白名单内")),
        }
    }

    // ---- 下载认证(WebDAV / 需要账号的直链)。整块 JSON 进 keyring,**写得进读不回**
    // (§7.7 凭证不过桥):前端拿不到数组,所以增删都由后端读改写,只回 host 清单。 ----

    /// 已配了认证的 host 清单(**只回 host,绝不回密码**)。
    pub fn http_creds_hosts(&self) -> Result<Vec<String>, AppError> {
        Ok(crate::web::load_http_creds(&self.store.settings)
            .into_iter()
            .map(|c| c.host)
            .collect())
    }

    /// 加一条 / 改一条(按 host 覆盖,大小写不敏感)。
    pub fn set_http_cred(&self, host: &str, user: &str, password: &str) -> Result<(), AppError> {
        let host = host.trim().trim_start_matches("http://").trim_start_matches("https://");
        let host = host.split('/').next().unwrap_or("").trim().to_string();
        if host.is_empty() {
            return Err(AppError::internal("要填网站地址(host)"));
        }
        let mut list = crate::web::load_http_creds(&self.store.settings);
        list.retain(|c| !c.host.eq_ignore_ascii_case(&host));
        list.push(crate::web::HttpCred {
            host,
            user: user.trim().to_string(),
            password: password.to_string(),
        });
        self.save_http_creds(&list)
    }

    /// 删一条。
    pub fn remove_http_cred(&self, host: &str) -> Result<(), AppError> {
        let mut list = crate::web::load_http_creds(&self.store.settings);
        list.retain(|c| !c.host.eq_ignore_ascii_case(host.trim()));
        self.save_http_creds(&list)
    }

    fn save_http_creds(&self, list: &[crate::web::HttpCred]) -> Result<(), AppError> {
        let json = if list.is_empty() {
            String::new()
        } else {
            serde_json::to_string(list).map_err(AppError::internal)?
        };
        crate::secrets::set(&self.store.settings, "net.http_creds", &json)
            .map_err(AppError::internal)
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::testkit::engine;

    /// **公钥在库里、私钥在密钥串里 → 换机恢复后不能装作没事(2026-08-22 审计)**。
    ///
    /// 备份带得走库(公钥),带不走系统密钥串(私钥)。原先只查公钥在不在就早退,于是
    /// 恢复到另一台机器后密钥对**永久错配**:签 JWT 拿不到私钥,和风天气这类服务一路
    /// 静默降级,用户只觉得「天气怎么不准了」。现在要求两半都在,缺一半就重新生成一对。
    #[test]
    fn keypair_regenerates_when_only_the_public_half_survived_a_restore() {
        use crate::crypto::{KEY_ED25519_PRIVATE, KEY_ED25519_PUBLIC};
        let eng = engine("keypair");
        let first = eng.ensure_app_keypair().unwrap();
        assert!(!first.trim().is_empty());
        // 幂等:两半都在时不该乱换钥匙(用户可能已经把公钥登记到外部服务了)
        assert_eq!(eng.ensure_app_keypair().unwrap(), first, "都在就别动");

        // 模拟「换台机器从备份恢复」:库(公钥)跟过来了,密钥串(私钥)没有
        crate::secrets::delete(&eng.store().settings, KEY_ED25519_PRIVATE);
        assert_eq!(
            eng.store().settings.get(None, KEY_ED25519_PUBLIC).unwrap().as_deref(),
            Some(first.as_str()),
            "公钥还在(它就住在库里)"
        );
        let second = eng.ensure_app_keypair().unwrap();
        assert_ne!(second, first, "私钥没了就得重新生成一对,不能继续用配不上的那把");
        assert!(
            crate::secrets::get(&eng.store().settings, KEY_ED25519_PRIVATE)
                .is_some_and(|p| !p.trim().is_empty()),
            "新的私钥要落下来"
        );
    }
}
