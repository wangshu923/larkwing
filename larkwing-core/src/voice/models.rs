//! 语音模型目录与用时下载(PLAN §11):**模型 = 数据**,按「语言→最强组件」对应;
//! 包里无模型,首次用时下载到 `数据目录/voice/models/<id>/`(性质同 yt-dlp,宪法 §4)。
//! 下载复用 components 的镜像展开/断流超时/HUD 进度;sherpa 系模型 GitHub release
//! 直链走 gh 镜像,HF 资产国内优先 hf-mirror(2026-06-12 实测三源连通)。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::bus::Text;
use crate::components::{candidates, fetch_url_to};
use crate::tasks::{TaskHandle, Tasks};

/// 一个模型 = 一组文件;每个文件给多源候选(顺序即尝试顺序,github 的再按镜像展开)。
pub struct ModelSpec {
    /// 落盘目录名:`voice/models/<id>/`。
    pub id: &'static str,
    /// HUD 标题 key(文案在前端字典)。
    pub label_key: &'static str,
    pub files: &'static [ModelFile],
}

pub struct ModelFile {
    pub name: &'static str,
    pub urls: &'static [&'static str],
}

/// silero VAD(语言无关,~2MB;sherpa 官方 release 资产)。
pub const SILERO_VAD: ModelSpec = ModelSpec {
    id: "silero-vad",
    label_key: "task.download.voice_vad",
    files: &[ModelFile {
        name: "silero_vad.onnx",
        urls: &["https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx"],
    }],
};

/// 中文行默认 ASR:SenseVoice int8(多语 zh/yue/en/ja/ko,自带标点/ITN,~230MB)。
/// A 期实测三选一(FireRed / SenseVoice / Paraformer)的起点;换默认 = 改这份数据。
pub const ASR_SENSE_VOICE: ModelSpec = ModelSpec {
    id: "sense-voice-2024-07-17",
    label_key: "task.download.voice_asr",
    files: &[
        ModelFile {
            name: "model.int8.onnx",
            urls: &[
                "https://hf-mirror.com/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main/model.int8.onnx",
                "https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main/model.int8.onnx",
            ],
        },
        ModelFile {
            name: "tokens.txt",
            urls: &[
                "https://hf-mirror.com/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main/tokens.txt",
                "https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main/tokens.txt",
            ],
        },
    ],
};

/// tar.bz2 整包模型(KWS 的 HF 仓库是 gated,只有 GitHub release 包;~4MB 走 gh 镜像)。
pub struct TarModelSpec {
    pub id: &'static str,
    pub label_key: &'static str,
    pub url: &'static str,
    /// 要从包里抽出的文件(按 entry 路径后缀匹配,落盘用文件名本体)。
    pub files: &'static [&'static str],
}

/// 本地离线 TTS:melo-tts 中英双语 VITS(PLAN §11 D;断网兜底,~170MB)。
/// dict_dir 省略(lexicon 已覆盖);带 rule_fsts 让数字/日期/电话读对。
pub const TTS_VITS_MELO: TarModelSpec = TarModelSpec {
    id: "vits-melo-tts-zh_en",
    label_key: "task.download.voice_tts_offline",
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-melo-tts-zh_en.tar.bz2",
    files: &[
        "model.onnx",
        "lexicon.txt",
        "tokens.txt",
        "date.fst",
        "number.fst",
        "phone.fst",
    ],
};

/// 声纹 embedding:3D-Speaker CAM++ zh-cn(PLAN §11 D;192 维,26MB,单 onnx)。
pub const SPEAKER_CAMPP_ZH: ModelSpec = ModelSpec {
    id: "campplus-sv-zh",
    label_key: "task.download.voice_speaker",
    files: &[ModelFile {
        name: "campplus.onnx",
        // gh release 单文件(走 gh 镜像候选);仓名 typo "recongition" 是上游原样,实测可达
        urls: &["https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx"],
    }],
};

/// 中文唤醒词模型:zipformer WenetSpeech 3.3M(关键词写中文即用,不训练;PLAN §11 C)。
pub const KWS_ZIPFORMER_ZH: TarModelSpec = TarModelSpec {
    id: "kws-zipformer-wenetspeech-3.3m",
    label_key: "task.download.voice_kws",
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/kws-models/sherpa-onnx-kws-zipformer-wenetspeech-3.3M-2024-01-01.tar.bz2",
    files: &[
        "encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
        "decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
        "joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
        "tokens.txt",
    ],
};

pub struct VoiceModels {
    dir: PathBuf,
    tasks: Tasks,
    http: reqwest::Client,
    /// 并发去重:同时只有一个模型下载在跑(后到的等同一份结果)。
    lock: tokio::sync::Mutex<()>,
}

impl VoiceModels {
    pub fn new(dir: PathBuf, tasks: Tasks) -> VoiceModels {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        VoiceModels { dir, tasks, http, lock: tokio::sync::Mutex::new(()) }
    }

    /// 模型就绪:全部文件在位即返回目录;缺则带 HUD 下载(.part 原子就位,
    /// drop-safe:中途取消只留 .part 残文件,下次覆盖重下)。
    pub async fn ensure(&self, spec: &ModelSpec, mirrors: &[String]) -> Result<PathBuf> {
        let dir = self.dir.join(spec.id);
        if self.all_present(spec, &dir) {
            return Ok(dir);
        }
        let _guard = self.lock.lock().await;
        if self.all_present(spec, &dir) {
            return Ok(dir); // 排队期间别人下完了
        }
        let task = self.tasks.start("download", Text::new(spec.label_key));
        match self.download(spec, &dir, mirrors, &task).await {
            Ok(()) => {
                task.done();
                Ok(dir)
            }
            Err(e) => {
                task.fail("task.err.download", serde_json::Value::Null);
                Err(e)
            }
        }
    }

    /// 不触发下载的就绪检查(设置页状态行用)。
    pub fn is_ready(&self, spec: &ModelSpec) -> bool {
        self.all_present(spec, &self.dir.join(spec.id))
    }

    pub fn is_tar_ready(&self, spec: &TarModelSpec) -> bool {
        let dir = self.dir.join(spec.id);
        spec.files.iter().all(|f| dir.join(f).is_file())
    }

    /// tar.bz2 整包就绪:下载(gh 镜像候选)→ 解包抽取目标文件 → 原子就位。
    pub async fn ensure_tar(&self, spec: &TarModelSpec, mirrors: &[String]) -> Result<PathBuf> {
        let dir = self.dir.join(spec.id);
        if self.is_tar_ready(spec) {
            return Ok(dir);
        }
        let _guard = self.lock.lock().await;
        if self.is_tar_ready(spec) {
            return Ok(dir);
        }
        let task = self.tasks.start("download", Text::new(spec.label_key));
        let r = self.download_tar(spec, &dir, mirrors, &task).await;
        match r {
            Ok(()) => {
                task.done();
                Ok(dir)
            }
            Err(e) => {
                task.fail("task.err.download", serde_json::Value::Null);
                Err(e)
            }
        }
    }

    async fn download_tar(
        &self,
        spec: &TarModelSpec,
        dir: &Path,
        mirrors: &[String],
        task: &TaskHandle,
    ) -> Result<()> {
        tokio::fs::create_dir_all(dir).await?;
        let tarball = dir.join("model.tar.bz2.part");
        let mut last_err: Option<anyhow::Error> = None;
        let mut ok = false;
        for url in candidates(spec.url, mirrors) {
            let host = url.split('/').nth(2).unwrap_or("?").to_string();
            task.step("step.connect", serde_json::json!({ "host": host }));
            match fetch_url_to(&self.http, &url, &tarball, task).await {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(e) => {
                    tracing::warn!(url, err = %format!("{e:#}"), "模型包下载失败,换下一个源");
                    last_err = Some(e);
                }
            }
        }
        if !ok {
            return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("没有可用下载源")))
                .with_context(|| format!("下载 {} 失败", spec.id));
        }
        task.step("step.extract", serde_json::Value::Null);
        let (tar2, dir2) = (tarball.clone(), dir.to_path_buf());
        let wanted: Vec<&'static str> = spec.files.to_vec();
        tokio::task::spawn_blocking(move || extract_tar_bz2(&tar2, &dir2, &wanted))
            .await
            .context("解包任务挂了")??;
        tokio::fs::remove_file(&tarball).await.ok();
        for f in spec.files {
            anyhow::ensure!(dir.join(f).is_file(), "包里没找到 {f}");
        }
        Ok(())
    }

    fn all_present(&self, spec: &ModelSpec, dir: &Path) -> bool {
        spec.files.iter().all(|f| dir.join(f.name).is_file())
    }

    async fn download(
        &self,
        spec: &ModelSpec,
        dir: &Path,
        mirrors: &[String],
        task: &TaskHandle,
    ) -> Result<()> {
        tokio::fs::create_dir_all(dir).await?;
        for file in spec.files {
            let dest = dir.join(file.name);
            if dest.is_file() {
                continue;
            }
            let part = dir.join(format!("{}.part", file.name));
            let mut last_err: Option<anyhow::Error> = None;
            let mut ok = false;
            for url in file.urls.iter().flat_map(|u| candidates(u, mirrors)) {
                let host = url.split('/').nth(2).unwrap_or("?").to_string();
                task.step("step.connect", serde_json::json!({ "host": host }));
                match fetch_url_to(&self.http, &url, &part, task).await {
                    Ok(()) => {
                        ok = true;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(url, err = %format!("{e:#}"), "模型文件下载失败,换下一个源");
                        last_err = Some(e);
                    }
                }
            }
            if !ok {
                return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("没有可用下载源")))
                    .with_context(|| format!("下载 {}/{} 失败", spec.id, file.name));
            }
            tokio::fs::rename(&part, &dest).await?;
        }
        Ok(())
    }
}

/// 解 tar.bz2,把 entry 文件名命中 wanted 的抽到 dir(扁平落盘,丢弃包内目录层级)。
fn extract_tar_bz2(tarball: &Path, dir: &Path, wanted: &[&str]) -> Result<()> {
    let file = std::fs::File::open(tarball)?;
    let mut archive = tar::Archive::new(bzip2::read::BzDecoder::new(file));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        if wanted.contains(&name.as_str()) {
            let mut out = std::fs::File::create(dir.join(&name))?;
            std::io::copy(&mut entry, &mut out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_have_files_and_sources() {
        for spec in [&SILERO_VAD, &ASR_SENSE_VOICE] {
            assert!(!spec.files.is_empty());
            for f in spec.files {
                assert!(!f.urls.is_empty(), "{}/{} 没有下载源", spec.id, f.name);
            }
        }
    }

    #[test]
    fn ready_check_is_false_on_empty_dir() {
        let dir = std::env::temp_dir().join(format!("lw-voice-models-{}", std::process::id()));
        let models = VoiceModels::new(dir.clone(), Tasks::new(crate::bus::Bus::new()));
        assert!(!models.is_ready(&SILERO_VAD));
        assert!(!models.is_tar_ready(&KWS_ZIPFORMER_ZH));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tar_bz2_extraction_picks_wanted_files_flat() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("lw-tar-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tarball = dir.join("m.tar.bz2");
        {
            let f = std::fs::File::create(&tarball).unwrap();
            let enc = bzip2::write::BzEncoder::new(f, bzip2::Compression::fast());
            let mut tb = tar::Builder::new(enc);
            let mut header = tar::Header::new_gnu();
            let put = |tb: &mut tar::Builder<_>, header: &mut tar::Header, path: &str, body: &[u8]| {
                header.set_size(body.len() as u64);
                header.set_cksum();
                tb.append_data(header, path, body).unwrap();
            };
            put(&mut tb, &mut header, "pkg-dir/tokens.txt", b"a 1\nb 2\n");
            put(&mut tb, &mut header, "pkg-dir/skip.onnx", b"NOPE");
            put(&mut tb, &mut header, "pkg-dir/sub/enc.int8.onnx", b"BIN");
            tb.into_inner().unwrap().finish().unwrap().flush().unwrap();
        }
        extract_tar_bz2(&tarball, &dir, &["tokens.txt", "enc.int8.onnx"]).unwrap();
        assert_eq!(std::fs::read(dir.join("tokens.txt")).unwrap(), b"a 1\nb 2\n");
        assert_eq!(std::fs::read(dir.join("enc.int8.onnx")).unwrap(), b"BIN", "嵌套目录被压平");
        assert!(!dir.join("skip.onnx").exists(), "不在 wanted 里的不抽");
        std::fs::remove_dir_all(&dir).ok();
    }
}
