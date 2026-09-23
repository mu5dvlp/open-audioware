//! 音楽クロックの土台。
//!
//! 初期構築仕様 §4.4(M5, 確定): 音声スレッドが「楽曲ボイスをこれまで何フレーム
//! レンダリングしたか」を数え、これと出力デバイスのタイムスタンプとの相関から
//! 曲位置とホスト単調時刻の対応(`SongTimeAt`)を導く。
//!
//! M0 時点ではデバイスタイムスタンプとの相関・世代カウンタ(generation)・
//! seqlock スナップショットは未実装(M2/M4 で拡張する)。ここでは
//! 「音声コールバックが送出したフレーム数」を単調カウントする土台のみを置く。
//!
//! この値は音声スレッド(書き込み)とゲームスレッド(読み取り)の双方から
//! ロック無しでアクセスできる必要があるため、最初から `AtomicU64` で持つ。

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering, fence};

use crate::music::MusicState;

/// BGM ボイス(初期構築仕様『§2』M14, M4-3)の状態だけをロック無しで公開する。
///
/// `music_clock.rs::MusicClockPublisher` が seqlock を使うのは「複数フィールドの
/// 整合を取る」必要があるからだが、BGM は**クロックを持たない**(発行元は楽曲ボイス
/// 〔M2〕に固定。初期構築仕様『§2』M14)ため、公開すべき値はこの1フィールドだけで
/// 済む。単一の `AtomicU8` の load/store だけで書き手・読み手の整合が保証できるため、
/// seqlock は不要(`RenderedFrameCounter` と同じ「単一フィールドは単純な atomic で足りる」
/// 考え方)。
///
/// 書き手は**音声スレッドただ1つ**(`Mixer::render` 末尾)。読み手は任意のスレッドから
/// ロック無しで [`Self::read`] できる。
#[derive(Debug)]
pub struct BgmStatePublisher {
    state: AtomicU8,
}

impl Default for BgmStatePublisher {
    fn default() -> Self {
        Self::new()
    }
}

impl BgmStatePublisher {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(MusicState::Loading.to_u8()),
        }
    }

    /// BGM ボイスの現在の状態を公開する。音声スレッドから、レンダリング後に呼ぶこと。
    ///
    /// リアルタイム安全: アロケーションもロックも行わない(atomic ストア1回のみ)。
    pub fn write(&self, state: MusicState) {
        // Release: この直前までの(このコールバックで書いた)出力データが、読み手が
        // Acquire で state を観測した時点で見えているようにする(`RenderedFrameCounter::add`
        // と同じ片方向の制約)。
        self.state.store(state.to_u8(), Ordering::Release);
    }

    /// 現在の状態を取得する。どのスレッドからでも呼べる。
    pub fn read(&self) -> MusicState {
        MusicState::from_u8(self.state.load(Ordering::Acquire))
    }
}

/// 音声コールバックがこれまでにレンダリングしたフレーム数を数えるカウンタ。
///
/// - `add` は音声スレッドから呼ぶ(リアルタイム安全: ロック・アロケーション無し)。
/// - `get` はどのスレッドからでも呼べる(将来のクロック相関 API・FFI スナップショットの
///   読み出し元になる)。
#[derive(Debug, Default)]
pub struct RenderedFrameCounter {
    frames: AtomicU64,
}

impl RenderedFrameCounter {
    pub const fn new() -> Self {
        Self {
            frames: AtomicU64::new(0),
        }
    }

    /// レンダリング済みフレーム数を加算する。音声スレッドから呼ぶ想定。
    ///
    /// リアルタイム安全: アロケーションもロックも行わない。
    pub fn add(&self, frames: u64) {
        // Release: この加算より前に行われたバッファ書き込みが、
        // 他スレッドから `get` (Acquire) を経由して観測されたときに見えるようにする。
        self.frames.fetch_add(frames, Ordering::Release);
    }

    /// 現在のレンダリング済みフレーム数を取得する。
    pub fn get(&self) -> u64 {
        self.frames.load(Ordering::Acquire)
    }

    /// カウンタをリセットする(将来: 再オープン・テスト用途)。
    pub fn reset(&self) {
        self.frames.store(0, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 複数スレッドをビジーループさせる重量級テスト同士が同時に走ると、CPU コア数を
    /// 越えて奪い合いになり、書き手が critical section の途中で長時間プリエンプトされる
    /// (ドキュメント記載の病的ケース)が不自然に起きやすくなる。`cargo test` は既定で
    /// テスト関数を並行実行するため、この Mutex で該当テスト同士を直列化しておく。
    static HEAVY_CONCURRENCY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn starts_at_zero() {
        let counter = RenderedFrameCounter::new();
        assert_eq!(counter.get(), 0);
    }

    #[test]
    fn add_accumulates() {
        let counter = RenderedFrameCounter::new();
        counter.add(128);
        counter.add(256);
        assert_eq!(counter.get(), 384);
    }

    #[test]
    fn reset_returns_to_zero() {
        let counter = RenderedFrameCounter::new();
        counter.add(1_000);
        counter.reset();
        assert_eq!(counter.get(), 0);
    }

    #[test]
    fn is_shareable_across_threads() {
        use std::sync::Arc;
        let counter = Arc::new(RenderedFrameCounter::new());
        let writer = {
            let counter = Arc::clone(&counter);
            std::thread::spawn(move || {
                for _ in 0..1_000 {
                    counter.add(1);
                }
            })
        };
        writer.join().unwrap();
        assert_eq!(counter.get(), 1_000);
    }

    #[test]
    fn bgm_state_publisher_starts_at_loading() {
        let publisher = BgmStatePublisher::new();
        assert_eq!(publisher.read(), MusicState::Loading);
    }

    #[test]
    fn default_matches_new_for_bgm_state_publisher() {
        let default = BgmStatePublisher::default();
        let new = BgmStatePublisher::new();
        assert_eq!(default.read(), new.read());

        default.write(MusicState::Playing);
        new.write(MusicState::Playing);
        assert_eq!(default.read(), new.read());
    }

    #[test]
    fn bgm_state_publisher_write_is_reflected_in_read() {
        let publisher = BgmStatePublisher::new();
        for state in [
            MusicState::Loading,
            MusicState::Ready,
            MusicState::Playing,
            MusicState::Paused,
        ] {
            publisher.write(state);
            assert_eq!(publisher.read(), state);
        }
    }

    #[test]
    fn bgm_state_publisher_is_shareable_across_threads() {
        use std::sync::Arc;
        let publisher = Arc::new(BgmStatePublisher::new());
        let writer = {
            let publisher = Arc::clone(&publisher);
            std::thread::spawn(move || {
                publisher.write(MusicState::Playing);
            })
        };
        writer.join().unwrap();
        assert_eq!(publisher.read(), MusicState::Playing);
    }

    #[test]
    fn music_clock_publisher_starts_at_zero() {
        let publisher = MusicClockPublisher::new();
        let snapshot = publisher.snapshot();
        assert_eq!(snapshot.song_frames, 0);
        assert_eq!(snapshot.host_time_ns, 0);
        assert_eq!(snapshot.sample_rate, 0);
        assert_eq!(
            snapshot.state,
            MusicState::Loading,
            "before the first publish, state must default to Loading (matches MusicVoice::new)"
        );
        assert!(!snapshot.is_playing);
        assert_eq!(snapshot.generation, 0);
    }

    #[test]
    fn music_clock_publisher_publish_is_reflected_in_snapshot() {
        let publisher = MusicClockPublisher::new();
        publisher.publish(48_000, 123_456_789, 48_000, MusicState::Playing);

        let snapshot = publisher.snapshot();
        assert_eq!(snapshot.song_frames, 48_000);
        assert_eq!(snapshot.host_time_ns, 123_456_789);
        assert_eq!(snapshot.sample_rate, 48_000);
        assert_eq!(snapshot.state, MusicState::Playing);
        assert!(snapshot.is_playing);
        assert_eq!(snapshot.generation, 0);
    }

    /// `is_playing` は `state` から `publish` 内で導出される冗長フィールドだが、
    /// `Playing` 以外の3状態すべてで `false` になることを別途固定化しておく
    /// (`Playing` だけの1点比較では「Playing 以外は全部 true になる」ような
    /// 実装ミスを検出できないため)。
    #[test]
    fn music_clock_publisher_is_playing_is_true_only_for_playing_state() {
        let publisher = MusicClockPublisher::new();
        for state in [
            MusicState::Loading,
            MusicState::Ready,
            MusicState::Playing,
            MusicState::Paused,
        ] {
            publisher.publish(0, 0, 48_000, state);
            let snapshot = publisher.snapshot();
            assert_eq!(snapshot.state, state);
            assert_eq!(snapshot.is_playing, state == MusicState::Playing);
        }
    }

    #[test]
    fn music_clock_publisher_bump_generation_increments_and_preserves_other_fields() {
        let publisher = MusicClockPublisher::new();
        publisher.publish(1_000, 2_000, 44_100, MusicState::Playing);

        publisher.bump_generation();
        let snapshot = publisher.snapshot();
        assert_eq!(snapshot.generation, 1);
        // 世代を進めても相関点そのものは publish 時点の値のままであるはず
        // (bump_generation は generation だけを書き換える)。
        assert_eq!(snapshot.song_frames, 1_000);
        assert_eq!(snapshot.host_time_ns, 2_000);
        assert_eq!(snapshot.sample_rate, 44_100);
        assert_eq!(snapshot.state, MusicState::Playing);
        assert!(snapshot.is_playing);

        publisher.bump_generation();
        assert_eq!(publisher.snapshot().generation, 2);
    }

    #[test]
    fn music_clock_publisher_bump_generation_wraps_without_panicking() {
        let publisher = MusicClockPublisher::new();
        // 世代カウンタは wrapping_add で進める実装になっている。2^32 回ループする代わりに
        // 上限直前へ直接書き込んでから折り返しを確認する(同一ファイル内の子モジュールなので
        // private フィールドへ直接アクセスできる)。
        publisher.generation.store(u32::MAX, Ordering::Relaxed);

        publisher.bump_generation();
        assert_eq!(publisher.snapshot().generation, 0);

        publisher.bump_generation();
        assert_eq!(publisher.snapshot().generation, 1);
    }

    #[test]
    fn seed_generation_after_reopen_sets_generation_without_disturbing_other_fields() {
        // 再オープン専用のシード API(M3「Android AAudio 切断復旧」案A)。他のフィールドは
        // まだ何も publish されていない新しいクロックなので既定値のままのはず。
        let publisher = MusicClockPublisher::new();
        publisher.seed_generation_after_reopen(42);

        let snapshot = publisher.snapshot();
        assert_eq!(snapshot.generation, 42);
        assert_eq!(snapshot.song_frames, 0);
        assert_eq!(snapshot.host_time_ns, 0);
        assert_eq!(snapshot.state, MusicState::Loading);

        // その後の bump_generation は通常どおりシードした値から進む(特別扱いされる
        // フィールドではないことの確認)。
        publisher.bump_generation();
        assert_eq!(publisher.snapshot().generation, 43);
    }

    #[test]
    fn seed_generation_after_reopen_wraps_without_panicking() {
        let publisher = MusicClockPublisher::new();
        publisher.seed_generation_after_reopen(u32::MAX);
        publisher.bump_generation();
        assert_eq!(publisher.snapshot().generation, 0);
    }

    #[test]
    fn music_clock_publisher_concurrent_snapshots_stay_consistent_with_single_writer() {
        // seqlock が存在する理由そのものを検証するテスト。
        // song_frames と host_time_ns を個別の atomic のまま公開すると、「更新後の
        // song_frames」と「更新前の host_time_ns」が混ざった不整合なスナップショットを
        // 読んでしまいうる。ここでは書き手に host_time_ns = song_frames * 2 という
        // 決まった関係を publish し続けさせ、読み手が常にその関係を保ったまま
        // 読めることを確認する。
        use std::sync::Arc;

        // 他の重量級並行テストと同時に走らないようにする(コメント参照)。
        let _guard = HEAVY_CONCURRENCY_TEST_LOCK.lock().unwrap();

        const ITERATIONS: u64 = 100_000;

        let publisher = Arc::new(MusicClockPublisher::new());

        let writer = {
            let publisher = Arc::clone(&publisher);
            std::thread::spawn(move || {
                for song_frames in 0..ITERATIONS {
                    publisher.publish(song_frames, song_frames * 2, 48_000, MusicState::Playing);
                    // 実運用では書き手(音声コールバック)は1コールバックにつき1回、
                    // 数ミリ秒間隔でしか publish しない。ここで一切待たずに全力で
                    // publish し続けると、読み手側の再試行(MAX_READ_RETRIES)より
                    // 書き手の更新速度の方が同等かそれ以上に速くなってしまい、
                    // ドキュメント記載の「再試行を使い切って未整合値を返す」経路
                    // (病的ケース用の保険)を非現実的な頻度で踏んでしまう。
                    // 軽いビジーウェイトを挟んで書き手の速度を読み手より
                    // 十分遅くし、想定どおり1〜2回の再試行で読めるようにする。
                    for _ in 0..80 {
                        std::hint::spin_loop();
                    }
                }
            })
        };

        // 書き手が動いている間、メインスレッドから読み続ける。終了条件は書き手の完了
        // (is_finished → join)であり、経過時間には依存しない。
        //
        // 読み手をノーウェイトの純粋なビジーループにすると、同じキャッシュラインを
        // 奪い合い続けて書き手の critical section(本来ナノ秒オーダー)を異常に
        // 引き延ばしてしまい、`MAX_READ_RETRIES` 回のリトライで切り上げる想定外の
        // 経路(ドキュメント記載の「書き手がプリエンプトされ続けた場合の保険」)を
        // 不自然に踏みやすくなる。読み過ぎを避けるため毎回小さく待つ
        // (これは終了条件ではなく単なるバックオフなので、フレークの原因にはならない)。
        while !writer.is_finished() {
            let snapshot = publisher.snapshot();
            assert_eq!(
                snapshot.host_time_ns,
                snapshot.song_frames * 2,
                "seqlock が壊れ、更新途中の不整合なスナップショットが見えた"
            );
            std::thread::sleep(std::time::Duration::from_micros(10));
        }
        writer.join().unwrap();

        // 書き手が完全に終わった後の最終状態も整合しているはず。
        let snapshot = publisher.snapshot();
        assert_eq!(snapshot.host_time_ns, snapshot.song_frames * 2);
    }

    #[test]
    fn music_clock_publisher_multiple_readers_stay_consistent_with_single_writer() {
        // 上のテストと同じ不変条件が、複数の読み手が同時に読んでも保たれることを確認する
        // (seqlock の読み手はロック不要で任意数のスレッドから並行に呼べる設計のため)。
        use std::sync::Arc;

        // 他の重量級並行テストと同時に走らないようにする(定義箇所のコメント参照)。
        let _guard = HEAVY_CONCURRENCY_TEST_LOCK.lock().unwrap();

        const ITERATIONS: u64 = 100_000;
        const READER_COUNT: usize = 2;

        let publisher = Arc::new(MusicClockPublisher::new());
        let stop = Arc::new(AtomicBool::new(false));

        let writer = {
            let publisher = Arc::clone(&publisher);
            std::thread::spawn(move || {
                for song_frames in 0..ITERATIONS {
                    publisher.publish(song_frames, song_frames * 2, 48_000, MusicState::Playing);
                    // 実運用では書き手(音声コールバック)は1コールバックにつき1回、
                    // 数ミリ秒間隔でしか publish しない。ここで一切待たずに全力で
                    // publish し続けると、読み手側の再試行(MAX_READ_RETRIES)より
                    // 書き手の更新速度の方が同等かそれ以上に速くなってしまい、
                    // ドキュメント記載の「再試行を使い切って未整合値を返す」経路
                    // (病的ケース用の保険)を非現実的な頻度で踏んでしまう。
                    // 軽いビジーウェイトを挟んで書き手の速度を読み手より
                    // 十分遅くし、想定どおり1〜2回の再試行で読めるようにする。
                    for _ in 0..80 {
                        std::hint::spin_loop();
                    }
                }
            })
        };

        // 上のテストと同様、読み手は毎回小さく待ってビジーループの過度な
        // キャッシュライン競合を避ける(書き手の critical section を不自然に
        // 引き延ばして `MAX_READ_RETRIES` 経路を誘発しないようにするため。
        // 終了条件ではなく単なるバックオフなのでフレークの原因にはならない)。
        let readers: Vec<_> = (0..READER_COUNT)
            .map(|_| {
                let publisher = Arc::clone(&publisher);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let snapshot = publisher.snapshot();
                        assert_eq!(
                            snapshot.host_time_ns,
                            snapshot.song_frames * 2,
                            "seqlock が壊れ、複数読み手のいずれかが不整合なスナップショットを見た"
                        );
                        std::thread::sleep(std::time::Duration::from_micros(10));
                    }
                })
            })
            .collect();

        // 書き手の完了を join で確認してから読み手に停止を伝える(スリープには頼らない)。
        writer.join().unwrap();
        stop.store(true, Ordering::Relaxed);
        for reader in readers {
            reader.join().unwrap();
        }
    }
}

/// 音楽クロックのスナップショット(初期構築仕様 §4.4)。
///
/// C# 側はこれを基準に、任意のホスト時刻 `t` の曲時間を
/// `SongTimeAt(t) = song_frames / sample_rate + (t − host_time_ns)` と線形外挿する。
///
/// **世代([`Self::generation`])を跨いだ外挿・補間をしてはならない。** シーク・停止・
/// 再オープン(出力ルート変化)で曲位置は不連続に飛ぶため、不連続を跨いでなめらかに
/// 補間すると判定時刻そのものが歪む。単調性の保証は同一世代内でのみ与える。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MusicClockSnapshot {
    /// 楽曲ボイスをこれまでにレンダリングしたフレーム数(= 曲位置)。
    pub song_frames: u64,
    /// 上記フレーム数に対応するホスト単調時刻(ナノ秒)。
    pub host_time_ns: u64,
    /// 出力サンプルレート(Hz)。0 は「まだ確定していない」。
    pub sample_rate: u32,
    /// 楽曲ボイスの再生状態(初期構築仕様『§4.3』の4状態。`mw_music_state()` の実体)。
    ///
    /// `MusicVoice::state()` は音声スレッドの排他所有物である `Mixer` の内部にしか無く、
    /// 他スレッド(ゲームスレッド)からロック無しで読めるのはこの seqlock 経由の値だけ
    /// (`mixer.rs::Mixer::render` が毎コールバック末尾でここへ publish する)。
    pub state: MusicState,
    /// 楽曲が進行中か。ポーズ中・停止中は false。
    ///
    /// `state == MusicState::Playing` から機械的に導出できる冗長フィールドだが、
    /// あえて残してある。理由は2つ: (1) 後段の FFI で `MwMusicPosition` として
    /// C# へそのまま渡す blittable 構造体になる予定で、C# 側が毎回 enum 比較をせずに
    /// 直接 bool として読めた方が呼び出し側にとって扱いやすい、(2) `state` は
    /// `MusicState::from_u8` を経由するため理論上の未知値フォールバック
    /// (`Loading` 扱い)が起こりうるが、`is_playing` は `publish` 時点で
    /// `state == Playing` から直接計算してそのまま格納するため、そのフォールバックの
    /// 影響を受けない独立した bool として振る舞う。**導出元は常に `publish` 呼び出し時の
    /// `state` 引数**であり、両者が食い違うスナップショットが観測されることはない
    /// (同じ seqlock の書き込み区間で両方を計算・格納するため)。
    pub is_playing: bool,
    /// 世代カウンタ。不連続のたびに 1 増える。
    pub generation: u32,
}

/// 曲位置とホスト時刻の相関点を、ロック無しで公開する seqlock(初期構築仕様 §4.4)。
///
/// 書き手は**音声スレッドただ1つ**(単一書き手前提の seqlock)。読み手は任意のスレッドから
/// [`Self::snapshot`] で取得でき、ロックも待ちも発生しない。
///
/// ## なぜ seqlock か
///
/// スナップショットは複数のフィールドが**互いに整合している**必要がある
/// (`song_frames` が更新後・`host_time_ns` が更新前、という組み合わせを読むと、
/// C# 側の線形外挿がその差分ぶんまるごとずれる)。個々のフィールドを atomic にするだけでは
/// 構造体としての整合は取れない。かといって音声スレッドで mutex は取れない(§5.3)。
/// seqlock は「書き手は待たない・読み手は不整合を検出してやり直す」ため、この条件を満たす。
///
/// ## プロトコル
///
/// 書き手は更新の前後で `seq` を 1 ずつ進める。奇数 = 書き込み中。
/// 読み手は前後の `seq` が「等しくかつ偶数」であることを確認できたときだけ、
/// 読んだ値を整合が取れたものとして扱う。
#[derive(Debug)]
pub struct MusicClockPublisher {
    seq: AtomicU32,
    song_frames: AtomicU64,
    host_time_ns: AtomicU64,
    sample_rate: AtomicU32,
    /// `MusicState` の数値表現(`MusicState::to_u8`/`from_u8`)。enum 自体は atomic に
    /// できないため、seqlock の内側で保護する本体フィールドとしてはこの形で持つ
    /// (`is_playing` と食い違うスナップショットを作らないため、必ず `state` と同じ
    /// `write` 区間の中で一緒に書く。`MusicClockSnapshot::is_playing` のドキュメント参照)。
    state: AtomicU8,
    is_playing: AtomicBool,
    generation: AtomicU32,
}

impl Default for MusicClockPublisher {
    fn default() -> Self {
        Self::new()
    }
}

impl MusicClockPublisher {
    /// 読み手が整合を諦めるまでの再試行回数。
    ///
    /// 書き手の critical section は relaxed ストア数回(ナノ秒オーダー)なので、
    /// 現実には1〜2回で成功する。この上限は「音声スレッドが書き込みの途中で
    /// OS にプリエンプトされたまま戻ってこない」という病的なケースで、読み手が
    /// 無限にスピンしないための保険。
    ///
    /// 【調査メモ】書き手を待ち無しの tight loop で回し続ける「敵対的な」並行テスト
    /// (`clock::tests::music_clock_publisher_concurrent_snapshots_stay_consistent_with_single_writer`
    /// など)では、読み手の16回の再試行が書き手の更新速度に追いつけず、この保険の経路
    /// (整合しないまま最後に読んだ値を返す = torn read)を実際に踏むことがある。
    /// これはメモリオーダリングのバグではない(この定数を極端に大きくすると再現しなくなる
    /// ことで切り分け済み。バグなら再試行回数を増やしても直らないはず)。実運用の書き手
    /// (音声コールバック)は1コールバックにつき1回、数ミリ秒間隔でしか publish しないため、
    /// この経路に実質入らない。上記テストは書き手に軽いビジーウェイトを挟んでこの間隔を
    /// 実運用に近づけることで対処している。**次にこのテストが落ちたら、まずここを疑うこと**
    /// (この定数を一時的に大きくして再現しなくなるか確認すれば、同じ調査をやり直さずに
    /// 切り分けられる)。
    const MAX_READ_RETRIES: u32 = 16;

    pub const fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            song_frames: AtomicU64::new(0),
            host_time_ns: AtomicU64::new(0),
            sample_rate: AtomicU32::new(0),
            state: AtomicU8::new(MusicState::Loading.to_u8()),
            is_playing: AtomicBool::new(false),
            generation: AtomicU32::new(0),
        }
    }

    /// 相関点と楽曲状態を公開する。**音声スレッドから、レンダリング後に呼ぶ。**
    ///
    /// `is_playing` は引数に取らず、ここで `state == MusicState::Playing` から導出して
    /// 一緒に格納する(`MusicClockSnapshot::is_playing` のドキュメント参照。
    /// 呼び出し側が state と is_playing を別々に指定できると食い違いうるため、
    /// 単一の入力値から両方を計算する設計にしてある)。
    ///
    /// リアルタイム安全: アロケーションもロックも行わない(atomic ストアのみ)。
    pub fn publish(
        &self,
        song_frames: u64,
        host_time_ns: u64,
        sample_rate: u32,
        state: MusicState,
    ) {
        self.write(|| {
            self.song_frames.store(song_frames, Ordering::Relaxed);
            self.host_time_ns.store(host_time_ns, Ordering::Relaxed);
            self.sample_rate.store(sample_rate, Ordering::Relaxed);
            self.state.store(state.to_u8(), Ordering::Relaxed);
            self.is_playing
                .store(state == MusicState::Playing, Ordering::Relaxed);
        });
    }

    /// 曲位置の不連続(シーク・停止・再オープン)を宣言し、世代を1つ進める。
    ///
    /// **音声スレッドから呼ぶ。** 新しい相関点は続く [`Self::publish`] で公開される。
    /// 世代の更新と相関点の更新を別々の書き込みにしているのは、不連続の宣言を
    /// 「新しい位置が確定する前」に読み手へ届けたいため(読み手は世代が変わった時点で
    /// 外挿を打ち切れる)。
    pub fn bump_generation(&self) {
        self.write(|| {
            let next = self.generation.load(Ordering::Relaxed).wrapping_add(1);
            self.generation.store(next, Ordering::Relaxed);
        });
    }

    /// 再オープン(ミドルウェア内部でストリームを閉じて開き直す。初期構築仕様『§6』
    /// 【確定】)専用の世代シード。
    ///
    /// 新しい `MusicClockPublisher::new()` は常に `generation = 0` から始まる。
    /// 再オープンで作り直したクロックをそのまま使うと、たまたま直前のクロックが
    /// 最後に観測させた generation と同じ値になりうる(例: 一度もシークしていない曲は
    /// generation=0 のまま)——その場合 C# 側が「generation が変わっていない」と
    /// 誤認し、不連続を跨いで補間してしまう(『§4.4』の禁止事項)。
    ///
    /// **再オープン処理(ゲームスレッド)が、この新しいクロックをまだ誰にも
    /// 公開していない段階で一度だけ呼ぶこと。** 呼び出し側は直前のクロックの
    /// `snapshot().generation` に `wrapping_add(1)` した値を渡す想定
    /// (`crates/mw-ffi/src/handle.rs::Instance::attempt_reopen` 参照)——これにより
    /// 新しいクロックの最初の generation は「直前のクロックが観測させたどの値とも
    /// 異なる」ことが保証される。
    ///
    /// 音声スレッドとは無関係(ゲームスレッド専用)だが、他の書き込みと同じ seqlock
    /// プロトコルを通しておく([`Self::write`])。
    pub fn seed_generation_after_reopen(&self, generation: u32) {
        self.write(|| {
            self.generation.store(generation, Ordering::Relaxed);
        });
    }

    /// 現在のスナップショットを取得する。**どのスレッドからでも呼べる。**
    ///
    /// 整合が取れないまま [`Self::MAX_READ_RETRIES`] 回に達した場合は、最後に読んだ値を
    /// そのまま返す(各フィールドは atomic なので値自体は有効。構造体としての整合だけが
    /// 保証されない)。この経路に入るのは書き手がプリエンプトされ続けたときだけで、
    /// 実運用では 1 コールバックぶんの相関点のずれに留まる。
    pub fn snapshot(&self) -> MusicClockSnapshot {
        let mut snapshot = MusicClockSnapshot {
            song_frames: 0,
            host_time_ns: 0,
            sample_rate: 0,
            state: MusicState::Loading,
            is_playing: false,
            generation: 0,
        };

        for _ in 0..Self::MAX_READ_RETRIES {
            let before = self.seq.load(Ordering::Acquire);
            if !before.is_multiple_of(2) {
                // 書き込み中。値を読まずにやり直す。
                continue;
            }

            snapshot = MusicClockSnapshot {
                song_frames: self.song_frames.load(Ordering::Relaxed),
                host_time_ns: self.host_time_ns.load(Ordering::Relaxed),
                sample_rate: self.sample_rate.load(Ordering::Relaxed),
                state: MusicState::from_u8(self.state.load(Ordering::Relaxed)),
                is_playing: self.is_playing.load(Ordering::Relaxed),
                generation: self.generation.load(Ordering::Relaxed),
            };

            // 上のデータロードがこの後方チェックより後ろへ回り込まないようにする。
            // `load(_, Acquire)` は「これより後ろの操作が前へ回り込まない」という
            // 片方向の制約にしかならず、逆向き(データロードがこのチェックの後ろへ
            // 回り込む)は防げない。それが起きると検証の後にデータを読むことになり、
            // seqlock の意味が無くなる。`fence(Acquire)` で逆向きを塞ぐ
            // (このコメントを消してここを単純な `Acquire` ロードに戻さないこと)。
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                return snapshot;
            }
        }

        snapshot
    }

    /// seqlock の書き込み区間。前後で `seq` を進める。
    ///
    /// 奇数化のストアは `store(_, Release)` ではなく `Relaxed` + 明示的な
    /// `fence(Release)` にしている。`store(_, Release)` は「これより前の操作が
    /// 後ろへ回り込まない」という片方向の制約にしかならず、逆向き(続く `body()` の
    /// ストアがこのストアより先に他コアから見えてしまう)は防げない。ここで
    /// 塞ぎたいのはまさにその逆向きなので `fence(Release)` を使う(このコメントを
    /// 消してここを単純な `Release` ストアに戻さないこと)。偶数化する側のストアは
    /// 逆に「`body()` が見える前にこのストアが見えない」ことだけを保証すればよく、
    /// これは `store(_, Release)` が持つ向きそのものなので、そのままでよい。
    fn write(&self, body: impl FnOnce()) {
        let start = self.seq.load(Ordering::Relaxed);
        self.seq.store(start.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);

        body();

        self.seq.store(start.wrapping_add(2), Ordering::Release);
    }
}
