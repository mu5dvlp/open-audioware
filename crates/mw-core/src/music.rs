//! 楽曲ボイスの再生状態機械(初期構築仕様『§4.3 楽曲再生』, M2, 一部)。
//!
//! - 楽曲ボイスは**同時に1本のみ**(初期構築仕様『§2 決定事項サマリ』M14: クロックの正を
//!   1つに保つため)。このファイルはその1本ぶんの状態(`MusicVoice`)と、
//!   PCM の供給元を抽象化するトレイト(`MusicFrameSource`)を定義する。
//! - この段階(M2 のこの部分)では**デコードもスレッドも扱わない**。実際のストリーミング
//!   デコード(Symphonia)は後続作業で `MusicFrameSource` を実装する形で差し込む。
//!   ここではその実装が満たすべき契約と、契約さえ満たせば正しく動く再生状態機械だけを
//!   固定化する。
//! - 状態は初期構築仕様『§4.3』の4つ: `Loading` / `Ready` / `Playing` / `Paused`。
//!   楽曲が未設定(≒まだ一度も `prepare` していない)場合も `Loading` として扱う
//!   (「ロード中」と「そもそも何もロードしていない」を状態としては区別しない。
//!   どちらも「まだ鳴らせない」という点で呼び出し側から見た扱いは同じであるため)。
//! - **すべての音量変化・停止はランプを通す**(初期構築仕様 M13)。`play` / `resume_at` は
//!   フェードイン、`pause` / `stop` はフェードアウトしてから実際に止める。ループ境界の
//!   折り返しも同様にフェードアウト→フェードインを通す(初期構築仕様『§4.3』の
//!   「プレビュー区間ループ(フェードイン/アウト付き)」)。**`seek` はランプを経由しない**
//!   ——「変化」ではなく「位置の付け替え」であり、その不連続そのものを
//!   `MusicRenderOutcome::discontinuity` で呼び出し側(音楽クロックの世代カウンタ)に
//!   伝えるのが役割だからである。
//! - 一方で**自然終了(PCM 終端到達)にランプは不要**(既存 `voice.rs` の `stopping` と
//!   同じ考え方: ランプの対象は「変化」と「停止」であって、単なるデータ末尾到達では
//!   ない)。
//!
//! # リアルタイム安全性(初期構築仕様『§5.3』)
//!
//! `MusicVoice::render` はヒープアロケーション・ロック取得・IO・パニック経路
//! (`unwrap`/`expect`/添字パニック)を一切行わない。添字アクセスは `get`/`get_mut` 経由に
//! 統一し、出力バッファ長がフレーム境界でなくても・0 長でもパニックしない
//! (`mixer::Mixer::render` と同じ流儀)。`MusicFrameSource` の実装(将来の
//! Symphonia ストリーミングデコーダ)がこの規約を守ることは呼び出し側の責務だが、
//! `render` 自身は `source.read` が何を返しても安全に振る舞う。

use crate::config::Config;
use crate::format::CHANNELS;
use crate::ramp::{Ramp, ms_to_samples};

/// 楽曲ボイスの再生状態(初期構築仕様『§4.3』)。
///
/// 楽曲が未設定のとき(まだ一度も [`MusicVoice::prepare`] していない、または
/// [`MusicFrameSource::is_ready`] がまだ `false` を返している間)は `Loading` として扱う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MusicState {
    /// プリロール中、または楽曲が未設定。
    Loading,
    /// プリロール完了、再生開始待ち。
    Ready,
    /// 再生中(フェードイン/フェードアウトの途中も含む。§4.2 の `stopping` と同様、
    /// 状態そのものはランプが収束するまで `Playing` のまま)。
    Playing,
    /// ポーズ中(位置は凍結され、`render` は無音を書き続ける)。
    Paused,
}

/// 楽曲 PCM の供給元(初期構築仕様『§4.7 デコードとリサンプリング』)。
///
/// 後続作業で Symphonia のストリーミングデコーダ(専用デコードスレッド + リングバッファ)が
/// これを実装する。この段階ではテストのフェイク実装のみを使う。
///
/// 将来の実装はデコードスレッドとの間の SPSC リングバッファから取り出す形になり、
/// 取り出しはアトミック操作を伴う。**呼び出し側([`MusicVoice::render`])はバルクで読む**
/// (1回のコールバックにつき、チャンク境界の数だけ呼ぶ。1フレームずつ何十回も呼ぶような
/// 使い方はしない)。**実装側も1フレームずつ呼ばれる前提を置かないこと**
/// (`out` の長さが可変であることを前提に実装すること)。
pub trait MusicFrameSource {
    /// インターリーブ f32 ステレオの出力バッファへ、書けるだけ書いて
    /// **実際に書いたフレーム数**を返す。**非ブロッキング**(足りなければ少なく返す)。
    /// `out` は複数フレームぶんのバルクな読み出しを想定したサイズで渡される
    /// (呼び出し側は1フレームずつは呼ばない)。
    fn read(&mut self, out: &mut [f32]) -> usize;
    /// 楽曲の総フレーム数。不明なら None。
    fn total_frames(&self) -> Option<u64>;
    /// プリロールが済んで再生可能か。
    fn is_ready(&self) -> bool;
    /// 指定フレームへの再位置決めを要求する(非同期でよい。完了は is_ready で判る)。
    fn request_seek(&mut self, frame: u64);
}

/// `render` 呼び出し1回分の結果(初期構築仕様『§4.4』の世代カウンタ・
/// 『§4.6』の `MusicEnded`/`MusicLooped` イベントの土台)。
///
/// イベント通知そのもの(§4.6)は後続作業。ここでは音声コールバック経路から
/// アロケーション無しで返せる、素の事実だけを持たせる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MusicRenderOutcome {
    /// `Playing` として処理したフレーム数(無音のまま抜けた分は含まない)。
    pub rendered_frames: usize,
    /// `source.read` が要求フレーム数に満たなかった(アンダーラン)ぶんのフレーム数。
    /// 無音で埋めた上でここに計上する。
    pub underrun_frames: usize,
    /// 総フレーム数に到達し、自然終了した(状態は `Ready` へ遷移済み)。
    pub ended: bool,
    /// ループ区間の終端で開始位置へ折り返した。
    pub looped: bool,
    /// 曲位置が不連続に変化した(シーク・停止確定・ループ折り返し・巻き戻し再開)。
    /// 呼び出し側はこれを見て音楽クロックの世代カウンタ(`clock.rs`)を進める。
    pub discontinuity: bool,
}

impl MusicRenderOutcome {
    /// 2回に分けた [`MusicVoice::render`] 呼び出しの結果を1つにまとめる。
    ///
    /// 予約再生(初期構築仕様『§4.5』, M2-5)がバッファの途中サンプルで発火する場合、
    /// `Mixer::render` は「発火前(無音のまま)」「`play()` 呼び出し」「発火後」の
    /// 2回の `render` 呼び出しに分けてサンプル精度の開始位置を実現する。
    /// 各フィールドの結合はどちらを先に渡しても結果は変わらない(加算 or 論理和)。
    pub fn merge(self, other: Self) -> Self {
        Self {
            rendered_frames: self.rendered_frames + other.rendered_frames,
            underrun_frames: self.underrun_frames + other.underrun_frames,
            ended: self.ended || other.ended,
            looped: self.looped || other.looped,
            discontinuity: self.discontinuity || other.discontinuity,
        }
    }
}

/// ランプ収束後に実行する保留アクション(初期構築仕様 M13 の「変化と停止はランプを通す」を
/// 状態機械として表現したもの)。`pause`/`stop`/ループ折り返しはいずれも
/// 「まずゲインを 0 へ向けてランプし、収束したら実際の遷移を行う」という同じ形をしている
/// ため、1つの保留状態として共通化する(`voice.rs` の `stopping` フラグに相当するが、
/// 楽曲ボイスは3種類の「止まった後にやること」を区別する必要があるため enum にしてある)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingTransition {
    /// 何も保留していない(通常再生、またはランプは既に収束済み)。
    None,
    /// ゲインが 0 に収束したら `Paused` へ。
    Pause,
    /// ゲインが 0 に収束したら位置を 0 に戻し `Ready` へ。
    Stop,
    /// ゲインが 0 に収束したら指定フレームへ折り返し、フェードインを開始する。
    LoopWrap(u64),
}

/// 唯一の楽曲ボイス(初期構築仕様『§2』M14: 同時に1本のみ)。
///
/// 保持するのは状態・曲頭からの位置・ループ区間・音量の4つ(初期構築仕様『§4.3』)。
/// PCM そのものは持たない(`render` に都度渡される [`MusicFrameSource`] から読む)。
pub struct MusicVoice {
    state: MusicState,
    /// 曲頭からの再生位置(フレーム単位)。
    position_frames: u64,
    /// ループ区間 `(開始フレーム, 終了フレーム)`。`None` ならループしない。
    loop_region: Option<(u64, u64)>,
    /// 実際に出力へ適用するゲイン。`play`/`resume_at` のフェードイン、
    /// `pause`/`stop`/ループ折り返しのフェードアウトの両方をこの1本のランプで表現する
    /// (M13: 音量に関わる変化は必ずこれを経由させる)。
    gain: Ramp,
    /// ユーザーが `set_volume` で指定した「鳴っているときの目標音量」。
    ///
    /// `gain` 自体はポーズ/停止/ループ折り返しのたびに 0 へ向けて動くため、
    /// 「フェードアウト前の音量はいくつだったか」を `gain.target()` だけでは復元できない
    /// (0 で上書きされてしまう)。そのため目標音量は別フィールドとして持ち、
    /// フェードインのたびにここへ向けて `gain` を動かす。
    target_volume: f32,
    /// ランプ長(ミリ秒 → サンプル数)の換算に使うサンプルレート。
    /// `render` の呼び出し側が実際の出力デバイスのレートを持つ設計のため、
    /// [`Self::set_sample_rate`] で後から確定・更新できるようにしてある
    /// (`Renderer::set_sample_rate` と同じ考え方)。
    sample_rate: u32,
    /// ランプ収束後にやること(`None` なら何もしない)。
    pending_settle: PendingTransition,
    /// 次回の `render` で `MusicRenderOutcome::discontinuity` として報告すべき不連続が
    /// 発生済みかどうか。`seek`/`resume_at`/`stop`(ポーズ中からの即時停止)は
    /// `render` の外から呼ばれるコマンドなので、フラグに積んでおいて次の `render` で
    /// まとめて報告する。
    pending_discontinuity: bool,
}

impl MusicVoice {
    /// 新規に構築する。初期状態は `Loading`(楽曲未設定)。
    pub fn new(sample_rate: u32) -> Self {
        Self {
            state: MusicState::Loading,
            position_frames: 0,
            loop_region: None,
            gain: Ramp::new(0.0),
            target_volume: 1.0,
            sample_rate,
            pending_settle: PendingTransition::None,
            pending_discontinuity: false,
        }
    }

    /// 出力デバイスのサンプルレートを確定・更新する(`Renderer::set_sample_rate` と同じ
    /// 考え方。音声コールバックが動き出す前に呼ぶことを想定)。
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate;
    }

    /// 現在の状態。
    pub fn state(&self) -> MusicState {
        self.state
    }

    /// 曲頭からの現在位置(フレーム単位)。
    pub fn position_frames(&self) -> u64 {
        self.position_frames
    }

    /// 現在のループ区間。
    pub fn loop_region(&self) -> Option<(u64, u64)> {
        self.loop_region
    }

    /// `set_volume` で設定した、鳴っているときの目標音量。
    pub fn volume(&self) -> f32 {
        self.target_volume
    }

    /// 既定ランプ長(サンプル数)。初期構築仕様 M13: 既定は [`Config::DEFAULT_RAMP_MS`]。
    fn ramp_samples(&self) -> u32 {
        ms_to_samples(Config::DEFAULT_RAMP_MS, self.sample_rate)
    }

    /// 新しい楽曲の準備を開始する(初期構築仕様『§4.3』「準備(プリロール込み)」)。
    ///
    /// 位置・ループ区間・保留中の遷移をすべて捨てて `Loading` から仕切り直す。
    /// ユーザーが設定した目標音量(`target_volume`)は曲をまたいで保持する
    /// (曲が変わるたびに音量設定がリセットされると使い勝手が悪いため)。
    /// 実際に `Ready` へ遷移するのは、以後の `render` で
    /// `source.is_ready()` が `true` を返した時点。
    pub fn prepare(&mut self) {
        self.state = MusicState::Loading;
        self.position_frames = 0;
        self.loop_region = None;
        self.pending_settle = PendingTransition::None;
        self.pending_discontinuity = false;
        self.gain.set_immediate(0.0);
    }

    /// 再生を開始する(`Ready` からのみ有効。既定ランプでフェードインする)。
    ///
    /// それ以外の状態から呼ばれた場合は無視する(リアルタイム安全性のためパニックはしない。
    /// 呼び出し順序の誤りは呼び出し側 API 層(M4 以降)で検出・拒否する想定)。
    pub fn play(&mut self) {
        if self.state != MusicState::Ready {
            return;
        }
        self.state = MusicState::Playing;
        self.pending_settle = PendingTransition::None;
        // Ready は常にゲイン 0 の不変条件を保っている(自然終了・停止のたびに明示的に
        // 0 へ揃えているため)が、念のため明示しておく。
        self.gain.set_immediate(0.0);
        let ramp = self.ramp_samples();
        self.gain.set_target(self.target_volume, ramp);
    }

    /// ポーズする(`Playing` からのみ有効)。
    ///
    /// 即座には止めない。既定ランプでフェードアウトし、`render` 側でゲインが 0 に
    /// 収束した時点で `Paused` へ遷移する(ステップ変化でブツッと切らないため。M13)。
    pub fn pause(&mut self) {
        if self.state != MusicState::Playing {
            return;
        }
        self.pending_settle = PendingTransition::Pause;
        let ramp = self.ramp_samples();
        self.gain.set_target(0.0, ramp);
    }

    /// 停止する。
    ///
    /// - `Playing` 中: 既定ランプでフェードアウトしてから位置を 0 に戻し `Ready` へ
    ///   (`pause` と同じ「ランプ収束待ち」の形)。
    /// - `Paused` 中: 既にゲインは 0(ポーズ完了時点の不変条件)なので、
    ///   ランプを待たずその場で位置を 0 に戻し `Ready` へ。
    /// - `Ready`/`Loading` 中: 既に停止相当なので何もしない。
    pub fn stop(&mut self) {
        match self.state {
            MusicState::Playing => {
                self.pending_settle = PendingTransition::Stop;
                let ramp = self.ramp_samples();
                self.gain.set_target(0.0, ramp);
            }
            MusicState::Paused => {
                self.position_frames = 0;
                self.state = MusicState::Ready;
                self.gain.set_immediate(0.0);
                self.pending_discontinuity = true;
            }
            MusicState::Ready | MusicState::Loading => {}
        }
    }

    /// 巻き戻し付き再開(初期構築仕様『§4.3』: テンプレート仕様「中断対応」の
    /// 「数秒巻き戻し + カウントダウン再開」の受け皿)。
    ///
    /// `frames` へ再位置決めしたうえで再生を再開し、既定ランプでフェードインする。
    /// `source.request_seek` を直接呼ぶため `source` を受け取る(`seek` と同様。
    /// この段階では `MusicVoice` の外にデコードスレッドが無いため、位置決め要求は
    /// 呼び出し側が保持する `MusicFrameSource` へその場で転送する設計にしてある)。
    ///
    /// `Loading` 中(まだ何も準備できていない)は無視する。
    pub fn resume_at(&mut self, frames: u64, source: &mut dyn MusicFrameSource) {
        if self.state == MusicState::Loading {
            return;
        }
        self.position_frames = frames;
        source.request_seek(frames);
        self.pending_settle = PendingTransition::None;
        self.pending_discontinuity = true;
        self.state = MusicState::Playing;
        // 巻き戻し再開は常にフェードインさせる(直前の音量が何であれ 0 から始める)。
        self.gain.set_immediate(0.0);
        let ramp = self.ramp_samples();
        self.gain.set_target(self.target_volume, ramp);
    }

    /// シークする(曲位置の不連続。初期構築仕様『§4.3』)。
    ///
    /// **ランプは経由しない** —— `pause`/`stop` と違って「変化」ではなく「位置の
    /// 付け替え」そのものであり、ランプで滑らかにする対象ではない。不連続の発生自体を
    /// 次の `render` の `MusicRenderOutcome::discontinuity` で報告する
    /// (呼び出し側が音楽クロックの世代カウンタを進める入口)。
    pub fn seek(&mut self, frames: u64, source: &mut dyn MusicFrameSource) {
        self.position_frames = frames;
        source.request_seek(frames);
        // 進行中だったポーズ/停止/ループ折り返しのフェードは、位置そのものが変わった
        // 時点で前提が崩れるため破棄する(中途半端なフェード後に古い位置へ戻る事故を防ぐ)。
        self.pending_settle = PendingTransition::None;
        self.pending_discontinuity = true;
    }

    /// ループ区間を設定する(初期構築仕様『§4.3』選曲プレビュー用)。
    ///
    /// `start >= end` の不正な区間は無視し、ループ無し(`None`)として扱う
    /// (リアルタイム安全性のため、不正入力でもパニックしない)。
    pub fn set_loop(&mut self, region: Option<(u64, u64)>) {
        self.loop_region = match region {
            Some((start, end)) if start < end => Some((start, end)),
            _ => None,
        };
    }

    /// 鳴っているときの目標音量を設定する。
    ///
    /// `Playing` かつフェード中でなければ、既定ランプで現在のゲインから新しい音量へ
    /// 遷移させる(M13)。ポーズ中・準備中は聞こえていないので、次にフェードインする際の
    /// 目標値としてだけ記録する。
    pub fn set_volume(&mut self, volume: f32) {
        self.target_volume = volume;
        if self.state == MusicState::Playing && self.pending_settle == PendingTransition::None {
            let ramp = self.ramp_samples();
            self.gain.set_target(volume, ramp);
        }
    }

    /// `out`(インターリーブ f32 ステレオ)を埋める。
    ///
    /// - `Playing` 以外では無音を書き、位置は進めない。
    /// - `Playing` では `source` から**チャンク単位でバルクに**読み、ゲインを適用して
    ///   書き込む。`source.read` が要求に満たない場合はアンダーランとして無音で埋める。
    ///   チャンクは「出力バッファ残り・総フレーム数(自然終了)・ループ区間の終端」の
    ///   うち最も手前で切る(いずれも `source` のカーソルを不連続に付け替える必要が
    ///   ある境界なので、1回のバルク読みがそれを跨がないようにする)。**`source.read`
    ///   の呼び出し回数は1回の `render` につきチャンク数ぶん(通常1〜2回)に収まる**
    ///   ——`MusicFrameSource` の実装は将来デコードスレッドとの SPSC リングバッファに
    ///   なる想定で、取り出しがアトミック操作を伴うため、フレーム単位で何十回も
    ///   呼ぶのは音声コールバックの中では割に合わない(`mixer.rs`
    ///   が読むメモリ上の `Arc<SoundData>` とは事情が異なる)。
    /// - ゲインの適用自体(`gain.advance()`)は従来どおり1フレームずつ進める。
    ///   チャンク化は `source.read` の呼び出し粒度だけを変えるものであり、
    ///   ランプの形(フェードイン/アウトのカーブ)は一切変わらない。
    /// - 総フレーム数に到達したら自然終了(ランプ無しで即座に `Ready` へ)。
    /// - ループ区間の終端に近づいたら既定ランプでフェードアウトを開始し、
    ///   ちょうど終端に到達したタイミングで開始位置へ折り返してフェードインする
    ///   (区間の残り物理フレームがランプ長よりも十分に長いプレビュー用途を想定。
    ///   区間長がランプ長未満の極端なケースはフェードが短縮されるだけでパニックはしない)。
    ///   折り返しがチャンクの途中(アンダーランでゲインの収束が位置の到達より先行した
    ///   場合)で確定したときは、そのチャンクの残り(付け替え前のカーソルで読んでしまった
    ///   生 PCM)を捨てて無音で上書きし、次のチャンクから新しい位置で読み直す。
    ///
    /// # リアルタイム安全性
    /// ヒープアロケーション・ロック・パニック経路無し(§5.3)。`out.len()` が
    /// `CHANNELS` の倍数でなくても・0 長でも安全(`mixer::Mixer::render` と同じ流儀)。
    pub fn render(
        &mut self,
        out: &mut [f32],
        source: &mut dyn MusicFrameSource,
    ) -> MusicRenderOutcome {
        for sample in out.iter_mut() {
            *sample = 0.0;
        }

        let mut outcome = MusicRenderOutcome::default();

        if self.state == MusicState::Loading && source.is_ready() {
            self.state = MusicState::Ready;
        }

        // render の外(コマンド呼び出し)で発生した不連続は、ここでまとめて報告する。
        outcome.discontinuity = std::mem::take(&mut self.pending_discontinuity);

        if self.state != MusicState::Playing {
            return outcome;
        }

        let total_frames_requested = out.len() / CHANNELS;
        let mut frame_index = 0usize;

        while frame_index < total_frames_requested && self.state == MusicState::Playing {
            let total_frames = source.total_frames();

            // 自然終了(§4.3: ランプ不要、データ末尾到達は「変化」でも「停止」でもない)。
            // チャンクは総フレーム数を跨がないよう切ってあるため、ここに到達するのは
            // 常に「直前のチャンクでちょうど末尾まで読み切った」瞬間になる。
            if let Some(total) = total_frames
                && self.position_frames >= total
            {
                self.state = MusicState::Ready;
                self.gain.set_immediate(0.0);
                self.pending_settle = PendingTransition::None;
                outcome.ended = true;
                break;
            }

            // このチャンクで読める上限フレーム数を、出力バッファ残り・総フレーム数・
            // ループ終端のうち最も手前で決める。総フレーム数・ループ終端はどちらも
            // `source` のカーソルを付け替える(自然終了は読み止め、ループはシーク)
            // 必要がある境界なので、1回のバルク読みがそこを跨がないようにする。
            let remaining_out = total_frames_requested - frame_index;
            let cap_by_total = match total_frames {
                Some(total) => (total.saturating_sub(self.position_frames)) as usize,
                None => usize::MAX,
            };
            let cap_by_loop = match self.loop_region {
                Some((_, loop_end)) if self.position_frames < loop_end => {
                    (loop_end - self.position_frames) as usize
                }
                _ => usize::MAX,
            };
            let chunk_frames = remaining_out.min(cap_by_total).min(cap_by_loop);
            if chunk_frames == 0 {
                // 理論上到達しない防御的分岐(直前の自然終了チェックと `cap_by_loop` の
                // ガードで抜けているはず)。無限ループを避けるためここで打ち切る。
                break;
            }

            let base_start = frame_index * CHANNELS;
            let base_end = base_start + chunk_frames * CHANNELS;
            let Some(chunk_out) = out.get_mut(base_start..base_end) else {
                break; // 防御的(到達しない想定)。
            };

            // バルク読み: `source.read` の呼び出しはチャンクにつき1回だけ。
            let read_count = source.read(chunk_out);

            // このチャンク内で確定した(=以降処理しない)フレーム数。通常はチャンク全体を
            // 使い切るが、ループ折り返しやポーズ/停止がチャンクの途中で確定した場合は
            // その時点までに短縮される。
            let mut frames_finalized = chunk_frames;

            for i in 0..chunk_frames {
                // ループ終端手前でフェードアウトを開始する(終端ちょうどでゲインが 0 に
                // 収束するようランプ長を「終端までの残りフレーム数」に合わせる)。
                if self.pending_settle == PendingTransition::None
                    && let Some((loop_start, loop_end)) = self.loop_region
                {
                    let ramp_len = u64::from(self.ramp_samples());
                    if self.position_frames < loop_end
                        && self.position_frames + ramp_len >= loop_end
                    {
                        let remaining = (loop_end - self.position_frames) as u32;
                        self.pending_settle = PendingTransition::LoopWrap(loop_start);
                        self.gain.set_target(0.0, remaining);
                    }
                }

                if i < read_count {
                    self.position_frames += 1;
                } else {
                    outcome.underrun_frames += 1;
                }

                let gain = self.gain.advance();
                let base = i * CHANNELS;
                if let Some(l) = chunk_out.get_mut(base) {
                    *l *= gain;
                }
                if let Some(r) = chunk_out.get_mut(base + 1) {
                    *r *= gain;
                }
                outcome.rendered_frames += 1;

                let mut wrapped = false;
                if self.gain.is_settled() {
                    match self.pending_settle {
                        PendingTransition::None => {}
                        PendingTransition::Pause => {
                            self.state = MusicState::Paused;
                            self.pending_settle = PendingTransition::None;
                        }
                        PendingTransition::Stop => {
                            self.state = MusicState::Ready;
                            self.position_frames = 0;
                            outcome.discontinuity = true;
                            self.pending_settle = PendingTransition::None;
                        }
                        PendingTransition::LoopWrap(restart_at) => {
                            self.position_frames = restart_at;
                            source.request_seek(restart_at);
                            let ramp = self.ramp_samples();
                            self.gain.set_target(self.target_volume, ramp);
                            outcome.looped = true;
                            outcome.discontinuity = true;
                            self.pending_settle = PendingTransition::None;
                            wrapped = true;
                        }
                    }
                }

                // ポーズ/停止が確定した、またはループ折り返しでカーソルを付け替えた場合、
                // このチャンクの残り(付け替え前のカーソルで既に読んでしまった生 PCM)は
                // 無音で上書きして捨てる。折り返しの場合は次のチャンクで新しい位置から
                // 読み直すので、以降このチャンクを再び参照することはない
                // (ポーズ/停止の場合は render 自体をここで終える)。
                if wrapped || self.state != MusicState::Playing {
                    if let Some(tail) = chunk_out.get_mut((i + 1) * CHANNELS..) {
                        for sample in tail.iter_mut() {
                            *sample = 0.0;
                        }
                    }
                    frames_finalized = i + 1;
                    break;
                }
            }

            frame_index += frames_finalized;
        }

        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用のフェイク `MusicFrameSource`。
    ///
    /// - 既定では「フレーム番号をそのまま値として返す」(L=R=フレーム番号)ので、
    ///   `MusicVoice` 側の位置管理が正しいかどうかをサンプル値からも検証できる。
    /// - `constant_value` を設定すると常に一定値を返す(ランプの滑らかさを検証する際、
    ///   PCM 側の変化とランプ側の変化が混ざらないようにするため)。
    /// - `remaining_budget` で「1回の read で供給できる総フレーム数」を制限でき、
    ///   アンダーランを人為的に起こせる。
    /// - `read_calls` は `read` が呼ばれた回数(バルク読みになっているかどうかを
    ///   テストから直接検証するためのカウンタ)。
    struct FakeSource {
        total_frames: Option<u64>,
        ready: bool,
        cursor: u64,
        remaining_budget: usize,
        constant_value: Option<f32>,
        seek_calls: Vec<u64>,
        read_calls: usize,
    }

    impl FakeSource {
        fn new(total_frames: Option<u64>) -> Self {
            Self {
                total_frames,
                ready: true,
                cursor: 0,
                remaining_budget: usize::MAX,
                constant_value: None,
                seek_calls: Vec::new(),
                read_calls: 0,
            }
        }
    }

    impl MusicFrameSource for FakeSource {
        fn read(&mut self, out: &mut [f32]) -> usize {
            self.read_calls += 1;
            let want = out.len() / CHANNELS;
            let cap_by_total = match self.total_frames {
                Some(total) => (total.saturating_sub(self.cursor)) as usize,
                None => usize::MAX,
            };
            let n = want.min(self.remaining_budget).min(cap_by_total);
            for i in 0..n {
                let value = self.constant_value.unwrap_or(self.cursor as f32);
                out[i * CHANNELS] = value;
                out[i * CHANNELS + 1] = value;
                self.cursor += 1;
            }
            self.remaining_budget -= n;
            n
        }

        fn total_frames(&self) -> Option<u64> {
            self.total_frames
        }

        fn is_ready(&self) -> bool {
            self.ready
        }

        fn request_seek(&mut self, frame: u64) {
            self.cursor = frame;
            self.seek_calls.push(frame);
        }
    }

    /// テストで使う既定サンプルレート。既定ランプ(5ms)がちょうど5サンプルになり、
    /// フェードの途中経過を数えやすい。
    const TEST_SAMPLE_RATE: u32 = 1_000;

    fn ramp_len() -> usize {
        ms_to_samples(Config::DEFAULT_RAMP_MS, TEST_SAMPLE_RATE) as usize
    }

    /// `Loading` → (render 1回で) `Ready` → `play` → `Playing` まで進めたペアを返す。
    fn playing_voice() -> (MusicVoice, FakeSource) {
        let mut voice = MusicVoice::new(TEST_SAMPLE_RATE);
        let mut source = FakeSource::new(None);
        let mut warmup = vec![0.0; CHANNELS];
        voice.render(&mut warmup, &mut source);
        assert_eq!(voice.state(), MusicState::Ready);
        voice.play();
        (voice, source)
    }

    #[test]
    fn initial_state_is_loading_and_render_writes_silence_without_advancing() {
        let mut voice = MusicVoice::new(TEST_SAMPLE_RATE);
        let mut source = FakeSource::new(None);
        source.ready = false; // プリロール未完了

        let mut buf = vec![1.0; 8 * CHANNELS];
        let outcome = voice.render(&mut buf, &mut source);

        assert_eq!(voice.state(), MusicState::Loading);
        assert_eq!(voice.position_frames(), 0);
        assert!(buf.iter().all(|&s| s == 0.0));
        assert_eq!(outcome.rendered_frames, 0);
    }

    #[test]
    fn ready_then_play_transitions_to_playing_and_advances_position() {
        let mut voice = MusicVoice::new(TEST_SAMPLE_RATE);
        let mut source = FakeSource::new(None);

        let mut warmup = vec![0.0; 4 * CHANNELS];
        voice.render(&mut warmup, &mut source);
        assert_eq!(voice.state(), MusicState::Ready);

        voice.play();
        assert_eq!(voice.state(), MusicState::Playing);

        let mut buf = vec![0.0; 10 * CHANNELS];
        voice.render(&mut buf, &mut source);
        assert_eq!(voice.position_frames(), 10);
    }

    #[test]
    fn pause_fades_out_then_stops_and_freezes_position() {
        let (mut voice, mut source) = playing_voice();

        // フェードイン+定常再生を十分に進める。
        let mut buf = vec![0.0; 50 * CHANNELS];
        voice.render(&mut buf, &mut source);
        assert_eq!(voice.state(), MusicState::Playing);

        voice.pause();

        // ランプ長より短い間はまだ Playing のまま(フェード中)。
        let mut mid_fade = vec![0.0; (ramp_len() - 1) * CHANNELS];
        voice.render(&mut mid_fade, &mut source);
        assert_eq!(voice.state(), MusicState::Playing);

        // ランプが尽きるだけの余裕を与えれば Paused に収束する。
        let mut settle = vec![0.0; 10 * CHANNELS];
        voice.render(&mut settle, &mut source);
        assert_eq!(voice.state(), MusicState::Paused);

        let frozen = voice.position_frames();
        let mut after = vec![1.0; 5 * CHANNELS];
        voice.render(&mut after, &mut source);
        assert_eq!(
            voice.position_frames(),
            frozen,
            "paused voice must not advance"
        );
        assert!(after.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn resume_at_rewinds_position_and_fades_in() {
        let (mut voice, mut source) = playing_voice();
        let mut buf = vec![0.0; 50 * CHANNELS];
        voice.render(&mut buf, &mut source);

        voice.pause();
        let mut settle = vec![0.0; 20 * CHANNELS];
        voice.render(&mut settle, &mut source);
        assert_eq!(voice.state(), MusicState::Paused);

        voice.resume_at(3, &mut source);
        assert_eq!(voice.state(), MusicState::Playing);
        assert_eq!(voice.position_frames(), 3);
        assert_eq!(source.seek_calls.last(), Some(&3));

        // フェードインの最初のサンプルは 0 より大きく、目標音量未満(まだ収束していない)。
        source.constant_value = Some(1.0);
        let mut first = vec![0.0; CHANNELS];
        voice.render(&mut first, &mut source);
        assert!(first[0] > 0.0 && first[0] < 1.0);
    }

    #[test]
    fn seek_jumps_position_and_flags_discontinuity_and_requests_seek_on_source() {
        let (mut voice, mut source) = playing_voice();
        let mut buf = vec![0.0; 20 * CHANNELS];
        voice.render(&mut buf, &mut source);

        voice.seek(500, &mut source);
        assert_eq!(voice.position_frames(), 500);
        assert_eq!(source.seek_calls.last(), Some(&500));

        let mut after = vec![0.0; 4 * CHANNELS];
        let outcome = voice.render(&mut after, &mut source);
        assert!(
            outcome.discontinuity,
            "seek must be reported as a discontinuity"
        );
    }

    #[test]
    fn underrun_fills_remainder_with_silence_and_counts_it() {
        let (mut voice, mut source) = playing_voice();
        // フェードインを終わらせて定常再生域へ。
        let mut buf = vec![0.0; 20 * CHANNELS];
        voice.render(&mut buf, &mut source);

        source.constant_value = Some(1.0);
        source.remaining_budget = 3; // 3フレームぶんしか供給できない

        let mut out = vec![1.0; 8 * CHANNELS];
        let outcome = voice.render(&mut out, &mut source);

        assert_eq!(outcome.underrun_frames, 5);
        assert_eq!(outcome.rendered_frames, 8);
        for frame in 3..8 {
            assert_eq!(
                out[frame * CHANNELS],
                0.0,
                "underran frame must be silent (L)"
            );
            assert_eq!(
                out[frame * CHANNELS + 1],
                0.0,
                "underran frame must be silent (R)"
            );
        }
    }

    #[test]
    fn natural_end_stops_exactly_at_total_frames_and_returns_to_ready() {
        let mut voice = MusicVoice::new(TEST_SAMPLE_RATE);
        let mut source = FakeSource::new(Some(10));
        let mut warmup = vec![0.0; CHANNELS];
        voice.render(&mut warmup, &mut source);
        voice.play();

        let mut buf = vec![0.0; 20 * CHANNELS]; // 総フレーム数(10)より大きい要求
        let outcome = voice.render(&mut buf, &mut source);

        assert!(outcome.ended);
        assert_eq!(voice.state(), MusicState::Ready);
        assert_eq!(
            voice.position_frames(),
            10,
            "must stop exactly at total_frames"
        );
        assert!(
            voice.position_frames() <= 10,
            "position must never exceed total_frames"
        );
    }

    #[test]
    fn loop_region_wraps_repeatedly_at_the_end_boundary() {
        let (mut voice, mut source) = playing_voice();
        source.constant_value = Some(1.0);
        // ループ区間はフェードイン開始直後、位置がまだ区間内にあるうちに設定する
        // (区間より先に位置が進んでしまうと、そもそも境界に到達できない)。
        voice.set_loop(Some((0, 12)));
        assert_eq!(voice.loop_region(), Some((0, 12)));

        let mut loop_count = 0;
        for _ in 0..20 {
            let mut buf = vec![0.0; 4 * CHANNELS];
            let outcome = voice.render(&mut buf, &mut source);
            if outcome.looped {
                loop_count += 1;
            }
            assert!(
                voice.position_frames() < 12,
                "position must stay within the loop region"
            );
        }
        assert!(
            loop_count >= 2,
            "the loop must wrap more than once (repeatability)"
        );
    }

    #[test]
    fn pause_ramp_has_no_abrupt_step_between_adjacent_samples() {
        let (mut voice, mut source) = playing_voice();
        source.constant_value = Some(1.0);
        let mut warmup = vec![0.0; 20 * CHANNELS];
        voice.render(&mut warmup, &mut source);

        voice.pause();

        let len = ramp_len() + 2;
        let mut buf = vec![0.0; len * CHANNELS];
        voice.render(&mut buf, &mut source);

        // ステップの理論上限は 1/ramp_len(1サンプルあたりの線形ランプの刻み幅)。
        let max_step = 1.0 / ramp_len() as f32 + 1e-4;
        let mut prev = buf[0];
        for frame in 1..len {
            let l = buf[frame * CHANNELS];
            assert!(
                (prev - l).abs() <= max_step,
                "adjacent samples must not jump discontinuously: prev={prev}, cur={l}"
            );
            prev = l;
        }
        assert_eq!(prev, 0.0, "ramp must settle exactly at 0");
    }

    #[test]
    fn render_does_not_panic_on_odd_length_or_empty_buffers() {
        let (mut voice, mut source) = playing_voice();

        let mut odd_buf = vec![0.0; 7]; // CHANNELS(2) の倍数でない
        voice.render(&mut odd_buf, &mut source);

        let mut empty_buf: Vec<f32> = Vec::new();
        let outcome = voice.render(&mut empty_buf, &mut source);
        assert_eq!(outcome.rendered_frames, 0);
    }

    #[test]
    fn set_volume_while_paused_only_takes_effect_on_next_fade_in() {
        let (mut voice, mut source) = playing_voice();
        let mut warmup = vec![0.0; 20 * CHANNELS];
        voice.render(&mut warmup, &mut source);

        voice.pause();
        let mut settle = vec![0.0; 20 * CHANNELS];
        voice.render(&mut settle, &mut source);
        assert_eq!(voice.state(), MusicState::Paused);

        voice.set_volume(0.25);
        assert_eq!(voice.volume(), 0.25);

        voice.resume_at(0, &mut source);
        source.constant_value = Some(1.0);
        let mut settle2 = vec![0.0; 20 * CHANNELS];
        voice.render(&mut settle2, &mut source);
        let last = settle2[(20 - 1) * CHANNELS];
        assert!(
            (last - 0.25).abs() < 1e-6,
            "must fade in to the newly set volume"
        );
    }

    #[test]
    fn merge_outcome_sums_counts_and_ors_flags() {
        let a = MusicRenderOutcome {
            rendered_frames: 10,
            underrun_frames: 1,
            ended: false,
            looped: false,
            discontinuity: true,
        };
        let b = MusicRenderOutcome {
            rendered_frames: 20,
            underrun_frames: 2,
            ended: true,
            looped: false,
            discontinuity: false,
        };
        let merged = a.merge(b);
        assert_eq!(merged.rendered_frames, 30);
        assert_eq!(merged.underrun_frames, 3);
        assert!(merged.ended);
        assert!(!merged.looped);
        assert!(merged.discontinuity);
    }

    #[test]
    fn set_loop_rejects_invalid_region() {
        let mut voice = MusicVoice::new(TEST_SAMPLE_RATE);
        voice.set_loop(Some((10, 10)));
        assert_eq!(voice.loop_region(), None);
        voice.set_loop(Some((10, 5)));
        assert_eq!(voice.loop_region(), None);
        voice.set_loop(Some((5, 10)));
        assert_eq!(voice.loop_region(), Some((5, 10)));
    }

    /// レビュー指摘の回帰テスト: `render` は `source.read` を1フレームずつ呼ばず、
    /// チャンク単位でバルクに読む(将来デコードスレッドとの SPSC リングバッファに
    /// なったとき、アトミック操作を伴う取り出しを何十回も繰り返さないため)。
    #[test]
    fn render_reads_from_source_in_bulk_not_one_frame_at_a_time() {
        // ループ・総フレーム数が無ければチャンクは1本で済むはず。
        let (mut voice, mut source) = playing_voice();
        source.read_calls = 0;
        let mut buf = vec![0.0; 256 * CHANNELS];
        voice.render(&mut buf, &mut source);
        assert_eq!(
            source.read_calls, 1,
            "no loop/total boundary means a single bulk read for the whole buffer"
        );

        // ループが位置0から設定されている状態(=何度も境界を跨ぐ)でも、
        // 呼び出し回数はチャンク(=境界を跨いだ回数)ぶんに収まるはず。
        // 256フレームの要求に対しループ長32なら折り返しは最大でも数回程度であり、
        // 「1フレームずつ = 256回」には程遠いことを確認する。
        let mut fresh_voice = MusicVoice::new(TEST_SAMPLE_RATE);
        let mut fresh_source = FakeSource::new(None);
        fresh_source.constant_value = Some(1.0);
        fresh_voice.render(&mut [0.0; CHANNELS], &mut fresh_source); // Loading -> Ready
        fresh_voice.set_loop(Some((0, 32)));
        fresh_voice.play();

        fresh_source.read_calls = 0;
        let mut looping_buf = vec![0.0; 256 * CHANNELS];
        let outcome = fresh_voice.render(&mut looping_buf, &mut fresh_source);
        assert!(
            outcome.looped,
            "the loop must actually engage for this assertion to be meaningful"
        );
        assert!(
            fresh_source.read_calls < 32,
            "chunked bulk reads must stay far below one read per frame, got {}",
            fresh_source.read_calls
        );
    }
}
