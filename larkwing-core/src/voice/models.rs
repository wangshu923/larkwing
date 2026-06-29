//! 语音模型目录与用时下载(PLAN §11):**模型 = 数据**,按「语言→最强组件」对应;
//! 包里无模型,首次用时下载到 `数据目录/voice/models/<id>/`(性质同 yt-dlp,宪法 §4)。
//! 下载复用 components 的镜像展开/断流超时/HUD 进度;sherpa 系模型 GitHub release
//! 直链走 gh 镜像,HF 资产国内优先 hf-mirror(2026-06-12 实测三源连通)。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::bus::Text;
use crate::components::{candidates, fetch_candidates};
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

/// 可选 ASR ①:Whisper-small 多语版(非 .en;encoder+decoder+tokens 三件,int8 ~374MB)。
/// Whisper 是 autoregressive、CPU 上比 SenseVoice 慢(稍等一两秒),但对**儿童 / 非标准发音**
/// 的鲁棒性是候选里唯一有实证的(68 万小时多样语料;AGENT §7.5)。用户报「小朋友识别差」选它。
pub const ASR_WHISPER_SMALL: ModelSpec = ModelSpec {
    id: "whisper-small-multi",
    label_key: "task.download.voice_asr",
    files: &[
        ModelFile {
            name: "small-encoder.int8.onnx",
            urls: &[
                "https://hf-mirror.com/csukuangfj/sherpa-onnx-whisper-small/resolve/main/small-encoder.int8.onnx",
                "https://huggingface.co/csukuangfj/sherpa-onnx-whisper-small/resolve/main/small-encoder.int8.onnx",
            ],
        },
        ModelFile {
            name: "small-decoder.int8.onnx",
            urls: &[
                "https://hf-mirror.com/csukuangfj/sherpa-onnx-whisper-small/resolve/main/small-decoder.int8.onnx",
                "https://huggingface.co/csukuangfj/sherpa-onnx-whisper-small/resolve/main/small-decoder.int8.onnx",
            ],
        },
        ModelFile {
            name: "small-tokens.txt",
            urls: &[
                "https://hf-mirror.com/csukuangfj/sherpa-onnx-whisper-small/resolve/main/small-tokens.txt",
                "https://huggingface.co/csukuangfj/sherpa-onnx-whisper-small/resolve/main/small-tokens.txt",
            ],
        },
    ],
};

/// 可选 ASR ②:Whisper-tiny 多语版(弱机/轻量档,encoder+decoder+tokens,int8 ~100MB)。
/// 体积最小、占内存最少,给**很老 / 内存紧张**的机器留条退路;同 Whisper 架构(与 small/medium
/// 共用构造分支)。代价 = 自回归、对中文识别明显比 SenseVoice 糙(故定位「轻量」非「最准」)。
pub const ASR_WHISPER_TINY: ModelSpec = ModelSpec {
    id: "whisper-tiny-multi",
    label_key: "task.download.voice_asr",
    files: &[
        ModelFile {
            name: "tiny-encoder.int8.onnx",
            urls: &[
                "https://hf-mirror.com/csukuangfj/sherpa-onnx-whisper-tiny/resolve/main/tiny-encoder.int8.onnx",
                "https://huggingface.co/csukuangfj/sherpa-onnx-whisper-tiny/resolve/main/tiny-encoder.int8.onnx",
            ],
        },
        ModelFile {
            name: "tiny-decoder.int8.onnx",
            urls: &[
                "https://hf-mirror.com/csukuangfj/sherpa-onnx-whisper-tiny/resolve/main/tiny-decoder.int8.onnx",
                "https://huggingface.co/csukuangfj/sherpa-onnx-whisper-tiny/resolve/main/tiny-decoder.int8.onnx",
            ],
        },
        ModelFile {
            name: "tiny-tokens.txt",
            urls: &[
                "https://hf-mirror.com/csukuangfj/sherpa-onnx-whisper-tiny/resolve/main/tiny-tokens.txt",
                "https://huggingface.co/csukuangfj/sherpa-onnx-whisper-tiny/resolve/main/tiny-tokens.txt",
            ],
        },
    ],
};

/// 可选 ASR ③:Whisper-medium 多语版(encoder+decoder+tokens,int8 ~950MB)。比 small 对
/// 儿童 / 口音更准一档;代价 = autoregressive + 大,CPU 上**明显慢**(可能等几秒到十几秒)。
/// 仅在 small 还不够准时再上(同 Whisper 架构,共用构造分支)。
pub const ASR_WHISPER_MEDIUM: ModelSpec = ModelSpec {
    id: "whisper-medium-multi",
    label_key: "task.download.voice_asr",
    files: &[
        ModelFile {
            name: "medium-encoder.int8.onnx",
            urls: &[
                "https://hf-mirror.com/csukuangfj/sherpa-onnx-whisper-medium/resolve/main/medium-encoder.int8.onnx",
                "https://huggingface.co/csukuangfj/sherpa-onnx-whisper-medium/resolve/main/medium-encoder.int8.onnx",
            ],
        },
        ModelFile {
            name: "medium-decoder.int8.onnx",
            urls: &[
                "https://hf-mirror.com/csukuangfj/sherpa-onnx-whisper-medium/resolve/main/medium-decoder.int8.onnx",
                "https://huggingface.co/csukuangfj/sherpa-onnx-whisper-medium/resolve/main/medium-decoder.int8.onnx",
            ],
        },
        ModelFile {
            name: "medium-tokens.txt",
            urls: &[
                "https://hf-mirror.com/csukuangfj/sherpa-onnx-whisper-medium/resolve/main/medium-tokens.txt",
                "https://huggingface.co/csukuangfj/sherpa-onnx-whisper-medium/resolve/main/medium-tokens.txt",
            ],
        },
    ],
};

/// 可选 ASR ④:小红书 FireRedASR2-CTC int8(单文件 ~740MB)。CTC 在 CPU 上快(RTF~0.17),
/// 标准普通话**最准一档**;但对儿童**无实证**(成人 SOTA ≠ 对孩子最好,AGENT §7.5)——作为
/// 「中文最准」选项给用户,孩子那条仍主推 Whisper。备用源 = GitHub release 同名 .tar.bz2(已验)。
pub const ASR_FIRERED_CTC: ModelSpec = ModelSpec {
    id: "fire-red-asr2-ctc-2026-02-25",
    label_key: "task.download.voice_asr",
    files: &[
        ModelFile {
            name: "model.int8.onnx",
            urls: &[
                "https://hf-mirror.com/csukuangfj/sherpa-onnx-fire-red-asr2-ctc-zh_en-int8-2026-02-25/resolve/main/model.int8.onnx",
                "https://huggingface.co/csukuangfj/sherpa-onnx-fire-red-asr2-ctc-zh_en-int8-2026-02-25/resolve/main/model.int8.onnx",
            ],
        },
        ModelFile {
            name: "tokens.txt",
            urls: &[
                "https://hf-mirror.com/csukuangfj/sherpa-onnx-fire-red-asr2-ctc-zh_en-int8-2026-02-25/resolve/main/tokens.txt",
                "https://huggingface.co/csukuangfj/sherpa-onnx-fire-red-asr2-ctc-zh_en-int8-2026-02-25/resolve/main/tokens.txt",
            ],
        },
    ],
};

/// 选中的中文 ASR 档(setting `voice.asr.model`,app 级)。默认 SenseVoice(快);加新档 =
/// 这里加一支 + 一个 `ModelSpec` + `asr.rs` 一个构造分支(架构不同),`X = 数据`(AGENT §1)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsrModel {
    SenseVoice,
    WhisperTiny,
    WhisperSmall,
    WhisperMedium,
    FireRedCtc,
}

impl AsrModel {
    /// setting 值 → 档(空 / 未知一律回落默认 SenseVoice;值是契约,与前端/engine 校验同源)。
    pub fn from_setting(s: &str) -> AsrModel {
        match s {
            "whisper-tiny" => AsrModel::WhisperTiny,
            "whisper-small" => AsrModel::WhisperSmall,
            "whisper-medium" => AsrModel::WhisperMedium,
            "firered-ctc" => AsrModel::FireRedCtc,
            _ => AsrModel::SenseVoice,
        }
    }

    /// 对应的下载规格(就绪检查 / 用时下载都认它)。
    pub fn spec(self) -> &'static ModelSpec {
        match self {
            AsrModel::SenseVoice => &ASR_SENSE_VOICE,
            AsrModel::WhisperTiny => &ASR_WHISPER_TINY,
            AsrModel::WhisperSmall => &ASR_WHISPER_SMALL,
            AsrModel::WhisperMedium => &ASR_WHISPER_MEDIUM,
            AsrModel::FireRedCtc => &ASR_FIRERED_CTC,
        }
    }
}

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

/// 目录保留式整包模型(ZipVoice 需要 `espeak-ng-data` 子目录,扁平抽取放不下;PLAN §11 D-clone)。
/// 解包 = strip-components=1(剥掉包顶层目录)+ 跳过 `skip` 子目录(test_wavs 类大样本省空间)。
pub struct TreeModelSpec {
    pub id: &'static str,
    pub label_key: &'static str,
    pub url: &'static str,
    /// 解包后用于就绪判定的相对路径(文件或目录均可)。
    pub ready: &'static [&'static str],
    /// 解包时跳过的(strip 后)顶层子目录名(省空间)。
    pub skip: &'static [&'static str],
    /// 包外单文件(不在 tar 里,如 vocos 声码器走 vocoder-models release):解包后单独拉进 dir。
    pub extra: &'static [ModelFile],
}

/// 零样本音色克隆:ZipVoice distill int8 中英双语(PLAN §11 D-clone)。跨语种零样本
/// (英文参考音说中文)亦可。vocoder = vocos_24khz;num_steps=4 distill 档。
/// **真机 watch-item**:下载 URL 实测 / vocos 是否在包内 / espeak-ng-data 随包 / 总体积。
pub const TTS_ZIPVOICE: TreeModelSpec = TreeModelSpec {
    id: "zipvoice-distill-int8-zh-en",
    label_key: "task.download.voice_tts_clone",
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/sherpa-onnx-zipvoice-distill-int8-zh-en-emilia.tar.bz2",
    ready: &[
        "encoder.int8.onnx",
        "decoder.int8.onnx",
        "vocos_24khz.onnx",
        "tokens.txt",
        "lexicon.txt",
        "espeak-ng-data",
    ],
    skip: &["test_wavs"],
    // vocos 声码器不在模型 tar 里(实测:tar 仅含 encoder/decoder/tokens/lexicon/espeak-ng-data),
    // 单独从 vocoder-models release 拉(~54MB)。
    extra: &[ModelFile {
        name: "vocos_24khz.onnx",
        urls: &[
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/vocoder-models/vocos_24khz.onnx",
        ],
    }],
};

pub struct VoiceModels {
    dir: PathBuf,
    tasks: Tasks,
    net: crate::net::Client,
    /// 并发去重:同时只有一个模型下载在跑(后到的等同一份结果)。
    lock: tokio::sync::Mutex<()>,
}

impl VoiceModels {
    pub fn new(dir: PathBuf, tasks: Tasks) -> VoiceModels {
        let net = crate::net::Client::new(|b| b.connect_timeout(std::time::Duration::from_secs(10)));
        VoiceModels { dir, tasks, net, lock: tokio::sync::Mutex::new(()) }
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
        fetch_candidates(&self.net, &candidates(spec.url, mirrors), &tarball, task)
            .await
            .with_context(|| format!("下载 {} 失败", spec.id))?;
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

    pub fn is_tree_ready(&self, spec: &TreeModelSpec) -> bool {
        let dir = self.dir.join(spec.id);
        spec.ready.iter().all(|f| dir.join(f).exists())
    }

    /// 目录保留式整包就绪:下载(gh 镜像候选)→ strip-components=1 解包(跳过 skip 子目录)
    /// → 就绪校验。与 `ensure_tar` 同结构,差别只在解包保留目录层级(espeak-ng-data)。
    pub async fn ensure_tar_tree(
        &self,
        spec: &TreeModelSpec,
        mirrors: &[String],
    ) -> Result<PathBuf> {
        let dir = self.dir.join(spec.id);
        if self.is_tree_ready(spec) {
            return Ok(dir);
        }
        let _guard = self.lock.lock().await;
        if self.is_tree_ready(spec) {
            return Ok(dir);
        }
        let task = self.tasks.start("download", Text::new(spec.label_key));
        match self.download_tar_tree(spec, &dir, mirrors, &task).await {
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

    async fn download_tar_tree(
        &self,
        spec: &TreeModelSpec,
        dir: &Path,
        mirrors: &[String],
        task: &TaskHandle,
    ) -> Result<()> {
        tokio::fs::create_dir_all(dir).await?;
        let tarball = dir.join("model.tar.bz2.part");
        fetch_candidates(&self.net, &candidates(spec.url, mirrors), &tarball, task)
            .await
            .with_context(|| format!("下载 {} 失败", spec.id))?;
        task.step("step.extract", serde_json::Value::Null);
        let (tar2, dir2) = (tarball.clone(), dir.to_path_buf());
        let skip: Vec<&'static str> = spec.skip.to_vec();
        tokio::task::spawn_blocking(move || extract_tar_bz2_tree(&tar2, &dir2, &skip))
            .await
            .context("解包任务挂了")??;
        tokio::fs::remove_file(&tarball).await.ok();
        // 包外单文件(如 vocos 声码器):解包后单独拉进同一 model 目录。
        for file in spec.extra {
            let dest = dir.join(file.name);
            if dest.is_file() {
                continue;
            }
            let part = dir.join(format!("{}.part", file.name));
            let urls: Vec<String> = file.urls.iter().flat_map(|u| candidates(u, mirrors)).collect();
            fetch_candidates(&self.net, &urls, &part, task)
                .await
                .with_context(|| format!("下载 {}/{} 失败", spec.id, file.name))?;
            tokio::fs::rename(&part, &dest).await?;
        }
        for f in spec.ready {
            anyhow::ensure!(dir.join(f).exists(), "包里没找到 {f}");
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
            let urls: Vec<String> = file.urls.iter().flat_map(|u| candidates(u, mirrors)).collect();
            fetch_candidates(&self.net, &urls, &part, task)
                .await
                .with_context(|| format!("下载 {}/{} 失败", spec.id, file.name))?;
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

/// 解 tar.bz2,strip-components=1(剥掉包顶层目录)保留子目录结构落盘;
/// 跳过 `skip` 列出的(strip 后)顶层子目录名(省空间,如 test_wavs)。
fn extract_tar_bz2_tree(tarball: &Path, dir: &Path, skip: &[&str]) -> Result<()> {
    let file = std::fs::File::open(tarball)?;
    let mut archive = tar::Archive::new(bzip2::read::BzDecoder::new(file));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let is_dir = entry.header().entry_type().is_dir();
        let path = entry.path()?.into_owned();
        // strip-components=1:丢掉包顶层目录那一段
        let rel: PathBuf = path.components().skip(1).collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let top = rel.components().next().and_then(|c| c.as_os_str().to_str()).unwrap_or("");
        if skip.contains(&top) {
            continue;
        }
        // 纵深防御(tar-slip):拒绝含 .. / 绝对路径成分的 entry,防解包逃逸出 model 目录。
        if !rel_is_safe(&rel) {
            tracing::warn!(path = %rel.display(), "跳过危险 tar 路径");
            continue;
        }
        let out = dir.join(&rel);
        if is_dir {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::File::create(&out)?;
        std::io::copy(&mut entry, &mut f)?;
    }
    Ok(())
}

/// tar entry(strip 后)相对路径是否安全:不含 .. / 绝对路径成分,防 tar-slip 逃逸。
fn rel_is_safe(rel: &Path) -> bool {
    !rel.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_have_files_and_sources() {
        for spec in [
            &SILERO_VAD,
            &ASR_SENSE_VOICE,
            &ASR_WHISPER_TINY,
            &ASR_WHISPER_SMALL,
            &ASR_WHISPER_MEDIUM,
            &ASR_FIRERED_CTC,
        ] {
            assert!(!spec.files.is_empty());
            for f in spec.files {
                assert!(!f.urls.is_empty(), "{}/{} 没有下载源", spec.id, f.name);
            }
        }
        // 五档 ASR 与 from_setting 的值一一对应(契约同步:前端 option / engine 校验同此五值)。
        assert_eq!(AsrModel::from_setting("sense-voice").spec().id, ASR_SENSE_VOICE.id);
        assert_eq!(AsrModel::from_setting("whisper-tiny").spec().id, ASR_WHISPER_TINY.id);
        assert_eq!(AsrModel::from_setting("whisper-small").spec().id, ASR_WHISPER_SMALL.id);
        assert_eq!(AsrModel::from_setting("whisper-medium").spec().id, ASR_WHISPER_MEDIUM.id);
        assert_eq!(AsrModel::from_setting("firered-ctc").spec().id, ASR_FIRERED_CTC.id);
        assert_eq!(AsrModel::from_setting("").spec().id, ASR_SENSE_VOICE.id, "空=默认");
        assert_eq!(AsrModel::from_setting("bogus").spec().id, ASR_SENSE_VOICE.id, "未知=默认");
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

    #[test]
    fn tree_extraction_strips_top_dir_skips_and_blocks_traversal() {
        use std::io::Write;
        let base = std::env::temp_dir().join(format!("lw-tree-test-{}", std::process::id()));
        let dir = base.join("model");
        std::fs::create_dir_all(&dir).unwrap();
        let tarball = base.join("m.tar.bz2");
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
            put(&mut tb, &mut header, "pkg/tokens.txt", b"a 1\n");
            put(&mut tb, &mut header, "pkg/espeak-ng-data/phontab", b"PH");
            put(&mut tb, &mut header, "pkg/test_wavs/ref.wav", b"WAV");
            tb.into_inner().unwrap().finish().unwrap().flush().unwrap();
        }
        extract_tar_bz2_tree(&tarball, &dir, &["test_wavs"]).unwrap();
        assert_eq!(std::fs::read(dir.join("tokens.txt")).unwrap(), b"a 1\n", "顶层目录被剥掉");
        assert_eq!(
            std::fs::read(dir.join("espeak-ng-data/phontab")).unwrap(),
            b"PH",
            "子目录结构保留"
        );
        assert!(!dir.join("test_wavs").exists(), "skip 的子目录不落盘");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn rel_safety_blocks_traversal() {
        use std::path::Path;
        assert!(rel_is_safe(Path::new("espeak-ng-data/phontab")));
        assert!(rel_is_safe(Path::new("tokens.txt")));
        assert!(!rel_is_safe(Path::new("../escape.txt")));
        assert!(!rel_is_safe(Path::new("a/../../escape")));
        assert!(!rel_is_safe(Path::new("/etc/passwd")));
    }
}
