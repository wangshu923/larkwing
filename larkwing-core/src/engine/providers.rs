//! 大脑接入点:供应商注册表 / 模型清单与目录覆盖 / 余额。
//!
//! 「供应商 = 数据」(§4.4):这里只做**装配**——把 `llm.providers` 的 spec 建成
//! provider 实例、把目录标签与用户覆盖合到一起给 UI。协议方言在各 provider 的
//! `to_wire`/`parse_chunk`,不在这。

use super::*;

/// 内置预设:不可删、只可禁用;列表里永远露出(模板预填,用户按需改)。
const BUILTIN_PROVIDER_IDS: &[&str] = &["deepseek", "anthropic"];

/// 供应商卡片视图。钥匙永不明文过桥:`${ENV}` 引用原样展示(它不是秘密),
/// 明文只回尾 4 位掩码;key_set 看的是解析后的真值(引用挂空变量 = 没钥匙)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderView {
    pub id: String,
    pub name: String,
    pub protocol: String,
    pub base_url: String,
    pub model: String,
    pub enabled: bool,
    pub builtin: bool,
    pub key_masked: String,
    pub key_set: bool,
}

impl ProviderView {
    fn from_spec(spec: &ProviderSpec) -> Self {
        let raw = spec.api_key.trim();
        let key_set = !resolve_env(raw).trim().is_empty();
        let key_masked = if raw.is_empty() {
            String::new()
        } else if raw.contains("${") {
            raw.to_string()
        } else {
            let tail: String = raw.chars().skip(raw.chars().count().saturating_sub(4)).collect();
            format!("····{tail}")
        };
        ProviderView {
            id: spec.id.clone(),
            name: spec.name.clone(),
            protocol: spec.protocol.as_str().into(),
            base_url: spec.base_url.clone(),
            model: spec.model.clone(),
            enabled: spec.enabled,
            builtin: BUILTIN_PROVIDER_IDS.contains(&spec.id.as_str()),
            key_masked,
            key_set,
        }
    }
}

/// 某模型的目录猜测(「高级」里给占位用:None = 目录也不知道)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelGuess {
    pub tier: crate::llm::catalog::Tier,
    pub in_usd_per_m: Option<f64>,
    pub out_usd_per_m: Option<f64>,
    pub ctx_window_tokens: Option<u32>,
    pub billing: crate::llm::catalog::BillingMode,
    /// 目录猜的「能不能看图」(未知 = false,§6.3)。
    pub vision: bool,
}

/// 设置页「高级」一格的全貌:目录猜测(占位)+ 当前用户覆盖(值)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelMeta {
    pub guess: ModelGuess,
    pub over: Option<crate::llm::catalog::ModelOverride>,
}

/// 设置页模型下拉的一行:接入点报上来的模型 id + 目录贴的标签(档位 / 能不能看图 / 牌价,
/// 覆盖优先)。`known` = 目录认识它(排序靶前);不认识的照列在后 —— 真相源是服务商,不替它删。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChoice {
    pub id: String,
    pub known: bool,
    pub tier: crate::llm::catalog::Tier,
    pub vision: bool,
    pub in_usd_per_m: Option<f64>,
    pub out_usd_per_m: Option<f64>,
}

/// 保存供应商卡的入参:None = 不动;api_key 空串视同 None(掩码回显防误存)。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPatch {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub api_key: Option<String>,
}

/// 一个模型 id → 下拉一行:目录(含用户覆盖)贴档位 / 看图 / 牌价;目录不认识 = known:false。
fn model_choice(id: String) -> ModelChoice {
    use crate::llm::catalog as cat;
    let (in_usd_per_m, out_usd_per_m) = cat::prices_of(&id);
    ModelChoice {
        known: cat::lookup(&id).is_some(),
        tier: cat::tier_of(&id),
        vision: cat::supports_vision(&id),
        in_usd_per_m,
        out_usd_per_m,
        id,
    }
}

/// 目录认识的在前、不认识的在后;组内保持服务端原序(稳定分区,不按字母重排 —— Anthropic
/// 之类按新旧给的顺序有信息量)。
fn rank_model_choices(mut choices: Vec<ModelChoice>) -> Vec<ModelChoice> {
    choices.sort_by_key(|c| !c.known);
    choices
}

/// 按一条供应商配置拉清单并贴标签(已接入的卡与未接入的草稿共用一条路)。
async fn list_models_for(spec: &ProviderSpec) -> Result<Vec<ModelChoice>, AppError> {
    let ids = spec.build().list_models().await?;
    Ok(rank_model_choices(ids.into_iter().map(model_choice).collect()))
}

/// 候选里最便宜的一档(catalog tier 最低;`Light < Balanced < Smart`)。同档并列保持候选序
/// (`min_by_key` 取首个最小项 → 沿用用户排的顺序,行为可预期)。抽成自由函数 → 可脱 Engine 单测。
/// 与 `registry::candidates(Strategy::Thrifty)` 同义(都按 `tier_of(model)` 排),只是作用在已建好的 provider 上。
pub(super) fn cheapest_candidate(
    candidates: &[(String, Arc<dyn LlmProvider>)],
) -> Option<&Arc<dyn LlmProvider>> {
    candidates
        .iter()
        .min_by_key(|(_, p)| crate::llm::catalog::tier_of(p.model_id()))
        .map(|(_, p)| p)
}

impl Engine {
    /// 直接注入单 provider(测试 / FakeLlm);None = 清空回首跑态。
    pub fn set_provider(&self, p: Option<Arc<dyn LlmProvider>>) {
        *self.llm.wr() = match p {
            Some(p) => vec![("custom".into(), p)],
            None => Vec::new(),
        };
    }

    pub fn has_provider(&self) -> bool {
        !self.llm.rd().is_empty()
    }

    /// 从 settings 重建供应商候选(开机装配 / 任何 llm.* 配置变更后调用)。
    /// 解析顺序:LARKWING_FAKE_LLM → llm.providers JSON → 单 DeepSeek 兜底
    /// (key: env DEEPSEEK_API_KEY → llm.api_key)。坏 JSON 走兜底并记日志,绝不让 app 哑掉。
    pub fn reload_providers(&self) -> Result<(), AppError> {
        // 模型覆盖(「高级」里改的档位/价/窗口)推入 catalog overlay —— boot 与任何配置变更都刷新一次。
        crate::llm::catalog::set_overrides(self.load_model_overrides());
        if std::env::var("LARKWING_FAKE_LLM").ok().as_deref() == Some("1") {
            tracing::info!("LARKWING_FAKE_LLM=1,使用假 provider");
            self.set_provider(Some(Arc::new(crate::llm::fake::FakeLlm::default())));
            return Ok(());
        }
        let registry = self.load_registry()?;
        let strategy = Strategy::parse(
            self.store.settings.get(None, "llm.strategy")?.unwrap_or_default().as_str(),
        );
        let candidates: Vec<(String, Arc<dyn LlmProvider>)> = registry
            .candidates(strategy)
            .into_iter()
            .map(|spec| (spec.id.clone(), spec.build()))
            .collect();
        tracing::info!(
            n = candidates.len(),
            order = ?candidates.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            "供应商候选已重建"
        );
        *self.llm.wr() = candidates;
        Ok(())
    }

    fn load_registry(&self) -> Result<ProviderRegistry, AppError> {
        // 秘密走 keyring(回落 settings),不落 SQLite 明文(§6.3)
        if let Some(json) = crate::secrets::get(&self.store.settings, "llm.providers") {
            match ProviderRegistry::from_json(&json) {
                Ok(reg) => return Ok(reg),
                // 容错铁律:配置坏了降级跑,不让 7274 哑掉
                Err(e) => tracing::error!(err = %e, "llm.providers 解析失败,回落单 DeepSeek"),
            }
        }
        // 单 key 兜底:用户填过的钥匙优先;没填过则挂 ${DEEPSEEK_API_KEY} 引用 ——
        // env 兜底就此变成数据(resolve_env 取值时解析),落盘也不会泄明文。
        let key = crate::secrets::get(&self.store.settings, "llm.api_key")
            .filter(|k| !k.trim().is_empty())
            .unwrap_or_else(|| "${DEEPSEEK_API_KEY}".into());
        Ok(ProviderRegistry::deepseek_only(key))
    }

    /// 写 key 入库并重建候选(不搞热更新魔法)。
    /// 多供应商配置存在时,同步更新其中的 deepseek 条目 —— 两处永远说同一把钥匙。
    pub fn set_api_key(&self, key: &str) -> Result<(), AppError> {
        let key = key.trim();
        if key.is_empty() {
            return Err(AppError { kind: ErrorKind::BadApiKey, message: "key 为空".into() });
        }
        crate::secrets::set(&self.store.settings, "llm.api_key", key).map_err(AppError::internal)?;
        if let Some(json) = crate::secrets::get(&self.store.settings, "llm.providers") {
            if let Ok(reg) = ProviderRegistry::from_json(&json) {
                let mut specs = reg.specs().to_vec();
                match specs.iter_mut().find(|s| s.id == "deepseek") {
                    Some(ds) => {
                        ds.api_key = key.to_string();
                        ds.enabled = true; // 贴钥匙 = 要用它;条目曾被禁用也就地复活
                    }
                    None => specs.push(ProviderSpec::deepseek(key.to_string())),
                }
                crate::secrets::set(
                    &self.store.settings,
                    "llm.providers",
                    &ProviderRegistry::new(specs).to_json(),
                )
                .map_err(AppError::internal)?;
            }
        }
        self.reload_providers()
    }

    /// 供应商卡片列表 = 生效中的注册表 + 还没配置的内置预设模板(全部预填,用户按需改)。
    pub fn list_providers(&self) -> Result<Vec<ProviderView>, AppError> {
        Ok(self.effective_specs()?.iter().map(ProviderView::from_spec).collect())
    }

    /// 按 id upsert 一张卡:None 字段不动;api_key 只在非空时替换(掩码回显防误存)。
    /// 保存即物化整张 llm.providers JSON(兜底注册表从此显式化)并重建候选。
    pub fn save_provider(&self, patch: ProviderPatch) -> Result<Vec<ProviderView>, AppError> {
        let invalid = |msg: &str| AppError { kind: ErrorKind::Internal, message: msg.into() };
        let id = patch.id.trim();
        if id.is_empty() || id.chars().count() > 32 {
            return Err(invalid("供应商 id 为空或过长"));
        }
        let mut specs = self.effective_specs()?;
        let spec = match specs.iter_mut().find(|s| s.id == id) {
            Some(s) => s,
            None => {
                specs.push(ProviderSpec { id: id.into(), name: id.into(), ..Default::default() });
                specs.last_mut().expect("刚 push 过")
            }
        };
        if let Some(name) = patch.name {
            let name = name.trim();
            if !name.is_empty() {
                spec.name = name.into();
            }
        }
        if let Some(p) = patch.protocol {
            spec.protocol = Protocol::parse(&p).ok_or_else(|| invalid("未知协议"))?;
        }
        if let Some(u) = patch.base_url {
            spec.base_url = u.trim().trim_end_matches('/').into();
        }
        if let Some(m) = patch.model {
            spec.model = m.trim().into();
        }
        if let Some(en) = patch.enabled {
            spec.enabled = en;
        }
        if let Some(k) = patch.api_key {
            let k = k.trim();
            if !k.is_empty() {
                spec.api_key = k.into();
            }
        }
        if spec.base_url.trim().is_empty() || spec.model.trim().is_empty() {
            return Err(invalid("接入点和模型不能为空"));
        }
        self.persist_specs(&specs)?;
        Ok(specs.iter().map(ProviderView::from_spec).collect())
    }

    /// 内置预设只可禁用不可删;自定义卡可删。
    pub fn remove_provider(&self, id: &str) -> Result<Vec<ProviderView>, AppError> {
        if BUILTIN_PROVIDER_IDS.contains(&id) {
            return Err(AppError {
                kind: ErrorKind::Internal,
                message: "内置供应商不可删除,可以禁用".into(),
            });
        }
        let mut specs = self.effective_specs()?;
        specs.retain(|s| s.id != id);
        self.persist_specs(&specs)?;
        Ok(specs.iter().map(ProviderView::from_spec).collect())
    }

    /// 生效注册表 + 缺席的内置预设(模板形态,钥匙空、其余预填)。
    fn effective_specs(&self) -> Result<Vec<ProviderSpec>, AppError> {
        let mut specs = self.load_registry()?.specs().to_vec();
        if !specs.iter().any(|s| s.id == "anthropic") {
            specs.push(ProviderSpec::anthropic(String::new()));
        }
        Ok(specs)
    }

    fn persist_specs(&self, specs: &[ProviderSpec]) -> Result<(), AppError> {
        crate::secrets::set(
            &self.store.settings,
            "llm.providers",
            &ProviderRegistry::new(specs.to_vec()).to_json(),
        )
        .map_err(AppError::internal)?;
        self.reload_providers()
    }

    /// 读用户模型覆盖表(明文 settings,非秘密 → 不进 keyring/白名单)。坏 JSON → 空表(容错)。
    fn load_model_overrides(&self) -> Vec<crate::llm::catalog::ModelOverride> {
        self.store
            .settings
            .get(None, "llm.model_overrides")
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default()
    }

    /// 设置页「高级」一格:目录猜测(占位)+ 当前用户覆盖(值)。
    pub fn model_meta(&self, model: &str) -> ModelMeta {
        let g = crate::llm::catalog::lookup(model);
        let guess = ModelGuess {
            tier: g.map(|i| i.tier).unwrap_or(crate::llm::catalog::Tier::Balanced),
            in_usd_per_m: g.and_then(|i| i.in_usd_per_m),
            out_usd_per_m: g.and_then(|i| i.out_usd_per_m),
            ctx_window_tokens: g.and_then(|i| i.ctx_window_tokens),
            // 目录无计价列(当前 provider 都有前缀缓存)→ 猜测恒为默认「按量+缓存」。
            billing: crate::llm::catalog::BillingMode::default(),
            vision: g.map(|i| i.vision).unwrap_or(false),
        };
        let over = self
            .load_model_overrides()
            .into_iter()
            .find(|o| o.model.eq_ignore_ascii_case(model));
        ModelMeta { guess, over }
    }

    /// 设置页模型下拉:向某供应商的接入点拉「这把钥匙能用的模型」,再用目录贴标签。
    /// 真相源在服务商(不写死名单 —— 中转 / 自架 / 刚上线的都照实列);目录只管排序与标签:
    /// 认识的在前(组内保服务端原序),不认识的在后。Err 如实带 kind(没钥匙 / 钥匙不对 / 网络 /
    /// 端点不实现),UI 据此提示、模型框仍可手填(§3.5)。
    pub async fn list_models(&self, provider_id: &str) -> Result<Vec<ModelChoice>, AppError> {
        let spec = self
            .load_registry()?
            .specs()
            .iter()
            .find(|s| s.id == provider_id)
            .cloned()
            .ok_or_else(|| AppError {
                kind: ErrorKind::NotFound,
                message: format!("没有这个供应商: {provider_id}"),
            })?;
        list_models_for(&spec).await
    }

    /// 同上,但对着**还没接入的草稿配置**(「自己接一个大脑」卡:选了预设、贴了钥匙、还没点接入)
    /// 现拉清单 —— 预设刻意不带默认模型(2026-09-04 用户拍板不写死),模型就在这一步选。
    /// 钥匙是前端 → 后端方向(与保存同向),不违「凭证不过桥」;`${ENV}` 引用照样在 build 时解析。
    pub async fn list_models_draft(
        &self,
        protocol: &str,
        base_url: &str,
        api_key: &str,
    ) -> Result<Vec<ModelChoice>, AppError> {
        let protocol = Protocol::parse(protocol).ok_or_else(|| AppError {
            kind: ErrorKind::Internal,
            message: format!("未知协议: {protocol}"),
        })?;
        let spec = ProviderSpec {
            id: "draft".into(),
            protocol,
            base_url: base_url.trim().into(),
            api_key: api_key.trim().into(),
            ..Default::default()
        };
        list_models_for(&spec).await
    }

    /// upsert 一条模型覆盖(空壳 = 删该条,回落纯目录)。持久化 + 刷新 overlay + 按新档位重排候选
    /// (档位影响路由顺序)。键 = 用户填的 model id;不在白名单、明文存。
    pub fn set_model_override(
        &self,
        over: crate::llm::catalog::ModelOverride,
    ) -> Result<(), AppError> {
        let model = over.model.trim().to_string();
        if model.is_empty() {
            return Ok(()); // 没指定模型 = 无事可做
        }
        let mut all = self.load_model_overrides();
        all.retain(|o| !o.model.eq_ignore_ascii_case(&model));
        let entry = crate::llm::catalog::ModelOverride { model, ..over };
        if !entry.is_empty() {
            all.push(entry);
        }
        let json = serde_json::to_string(&all).map_err(AppError::internal)?;
        self.store
            .settings
            .set(None, "llm.model_overrides", &json)
            .map_err(AppError::internal)?;
        self.reload_providers() // 刷新 overlay + 档位变了重排候选
    }

    // ---- 多用户 / 家人(PLAN §11 D;会话管理类一等公民,§4 永不委托可插拔层) ----

    /// 主选供应商的账户余额。None = 没配供应商/不支持/查不到 —— 锦上添花,失败静默。
    /// 查到的值顺手落快照(变了才记):余额差值 = 供应商账面的真实花费,给分析对账用。
    pub async fn llm_balance(&self) -> Option<crate::llm::AccountBalance> {
        // 锁内只取 Arc 快照,await 在锁外(RwLock guard 不能跨 await)
        let (provider_id, provider) = {
            let candidates = self.llm.rd();
            candidates.first().map(|(id, p)| (id.clone(), p.clone()))
        }?;
        let balance = provider.balance().await?;
        let (store, b) = (self.store.clone(), balance.clone());
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = store.usage.add_balance_snapshot(&provider_id, &b.currency, &b.amount)
            {
                tracing::warn!("余额快照落库失败: {e:#}");
            }
        })
        .await;
        Some(balance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// cheap-model 路由(§13.6 变体 A):后台提炼挑**最便宜档** provider,无视候选序(聊天用脑策略);
    /// 模型下拉排序:目录认识的靶前(组内保服务端原序)、不认识的沉底;标签取自目录(档位 / 看图 / 牌价)。
    #[test]
    fn model_choices_rank_known_first_and_keep_server_order() {
        let ids =
            ["text-embedding-3-small", "gpt-5", "whisper-1", "gpt-5-mini", "deepseek-v4-flash-vision-exp"];
        let ranked = rank_model_choices(ids.iter().map(|s| model_choice(s.to_string())).collect());
        let order: Vec<&str> = ranked.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            order,
            ["gpt-5", "gpt-5-mini", "deepseek-v4-flash-vision-exp", "text-embedding-3-small", "whisper-1"]
        );
        assert!(ranked[0].known && !ranked[3].known && !ranked[4].known);
        assert_eq!(ranked[1].tier, crate::llm::catalog::Tier::Light);
        assert!(ranked[2].vision, "vision-exp 目录标能看图");
        assert_eq!(ranked[2].in_usd_per_m, Some(0.44));
        assert_eq!(ranked[3].in_usd_per_m, None, "目录不认识 → 不报价");
    }

    /// 同档并列取首个(沿用候选序);单 provider 选它自己(零回归);空 → None。
    #[test]
    fn cheapest_candidate_picks_lowest_tier() {
        let build = |id: &str, model: &str| -> (String, Arc<dyn LlmProvider>) {
            let spec = ProviderSpec {
                id: id.into(),
                name: id.into(),
                api_key: "sk-test".into(),
                model: model.into(),
                ..Default::default()
            };
            (id.into(), spec.build())
        };
        // 候选按「聪明优先」排(旗舰在前)→ 仍应挑出最便宜的 flash(Light)
        let cands = vec![
            build("smart", "claude-opus-4-8"),   // Smart
            build("mid", "deepseek-v4"),          // Balanced
            build("cheap", "deepseek-v4-flash"),  // Light
        ];
        assert_eq!(
            cheapest_candidate(&cands).unwrap().model_id(),
            "deepseek-v4-flash",
            "挑最低档,无视候选序"
        );
        // 同档并列 → 取首个(min_by_key 取首个最小项,可预期)
        let ties = vec![build("a", "deepseek-v4"), build("b", "deepseek-chat")];
        assert_eq!(cheapest_candidate(&ties).unwrap().model_id(), "deepseek-v4", "并列取首个");
        // 单 provider → 选它自己(与主选一致,零回归);空候选 → None
        let one = vec![build("solo", "claude-opus-4-8")];
        assert_eq!(cheapest_candidate(&one).unwrap().model_id(), "claude-opus-4-8");
        assert!(cheapest_candidate(&[]).is_none());
    }

    // —— conversation_trace（「想了想」历史回放）—— //
}
