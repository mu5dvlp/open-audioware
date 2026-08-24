//! C ABI 境界で使う値型(初期構築仕様 §5.5)。
//!
//! `#[repr(...)]` の enum は csbindgen が C# 側 enum として自動生成する
//! (`build.rs` がこのファイルも入力に含める)。
//!
//! ## M2-7 で csbindgen 入力に加えた理由
//!
//! `MwSoundMode`/`MwBus` はこれまで「extern 関数の引数型としては使わず(`i32` を
//! 受け取って `from_raw` で検証する)、値としても C# 側へ渡らない内部専用の型」
//! だったため、csbindgen の入力から意図的に外してあった(`crates/mw-ffi/CLAUDE.md`
//! 参照)。M2-7 でここに [`MwMusicState`]/[`MwMusicPosition`] を追加したことで
//! 事情が変わった: [`MwMusicPosition`] は `mw_music_get_position` の out 引数の
//! 実際の型として extern 関数シグネチャに現れる(`event.rs::MwEvent` と同じ理由で
//! C# 側の型を自動生成させる必要がある)。そのため `build.rs` の入力にこのファイルを
//! 追加した。[`MwMusicState`] 自体は(`MwBus` 等と同じ理由で)`mw_music_state()` の
//! 引数には直接使わず `i32` 出力のままだが、同じファイルにある以上どうせ一緒に
//! 読まれるので、C# 側に対応する enum を無料で生成させておく(呼び出し側が
//! 生の `int` をこの enum へキャストして使える)。

/// `mw_sound_load` の `mode` 引数。
///
/// M1 は `Se`(全デコード常駐)のみ実装する。`Music`(圧縮のまま保持しストリーミング
/// デコード)は M2 で実装する — M1 では `MwResult::ErrUnsupportedSoundMode` を返す
/// (初期構築仕様 §5.2「SE はロード時に全デコードしてメモリ常駐。楽曲のみストリーミング
/// デコード」)。
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MwSoundMode {
    Se = 0,
    Music = 1,
}

impl MwSoundMode {
    /// FFI 境界から渡された生の `i32` を検証・復元する。範囲外は `None`。
    pub fn from_raw(raw: i32) -> Option<Self> {
        match raw {
            0 => Some(MwSoundMode::Se),
            1 => Some(MwSoundMode::Music),
            _ => None,
        }
    }
}

/// 固定4本のバス識別子(`mw_core::BusId` の FFI 境界表現)。
///
/// 値は `mw_core::BusId` の `#[repr(u8)]` 判別子と1:1で一致させてある
/// (`from_core`/`to_core` は単純なキャストで往復できる)。
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MwBus {
    Master = 0,
    Bgm = 1,
    Se = 2,
    Voice = 3,
}

impl MwBus {
    pub fn from_raw(raw: i32) -> Option<Self> {
        match raw {
            0 => Some(MwBus::Master),
            1 => Some(MwBus::Bgm),
            2 => Some(MwBus::Se),
            3 => Some(MwBus::Voice),
            _ => None,
        }
    }

    pub fn to_core(self) -> mw_core::BusId {
        match self {
            MwBus::Master => mw_core::BusId::Master,
            MwBus::Bgm => mw_core::BusId::Bgm,
            MwBus::Se => mw_core::BusId::Se,
            MwBus::Voice => mw_core::BusId::Voice,
        }
    }
}

/// 楽曲ボイスの再生状態(初期構築仕様『§4.3 楽曲再生』『§5.5』の `mw_music_state()` の
/// 実体)。
///
/// 判別子は `mw_core::MusicState::to_u8`/`clock.rs::MusicClockPublisher` が
/// seqlock の内側で使う数値表現と1:1で一致させてある([`MwMusicState::from_core`])。
///
/// `mw_music_state` の `out_state` 引数自体は(`MwBus`/`MwSoundMode` と同じ理由で)
/// この enum を直接使わず素の `i32` として渡す——モジュール doc「enum を FFI 引数に
/// 直接使わない理由」参照(呼び出し側が範囲外の値を書き込める余地は無い out 専用
/// 引数とはいえ、既存の規約と揃えておく)。一方 [`MwMusicPosition::state`] は
/// この enum の値そのものを**フィールド型として**持つ——`event.rs::MwEvent::kind`
/// (`MwEventKind` を直接フィールドに持つ)と同じパターンで、Rust 側が書き込む値は
/// 常にこの enum の妥当な判別子であり、C# 側が不正値を書き込んでそれを Rust が
/// 読み返す経路が無いため、enum 引数を避ける理由(未定義動作)がそもそも当てはまらない。
///
/// **この構造体フィールドとしての利用が、csbindgen に C# `enum` を生成させる
/// 実際の仕掛けでもある**: 実際に生成してみて確認した挙動として、csbindgen は
/// 「`extern "C"` 関数シグネチャに直接現れる型」だけでなく「そこに現れる `struct` の
/// フィールド型」も再帰的に辿って C# 側の型を生成する(`MwEvent`/`MwEventKind` が
/// 既にそうなっている)一方、**単に同じファイルに置いてあるだけで、どこからも
/// 参照されない `pub enum`(`MwBus`/`MwSoundMode` がまさにこれ)は生成されない**。
/// そのため「C# 側に列挙を生成させたいだけ」で `mw_music_state` の引数型を
/// `*mut MwMusicState` に変える必要は無く(むしろそれは避けたい規約違反になる)、
/// [`MwMusicPosition::state`] の型として使うだけで目的を達成できる。
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MwMusicState {
    Loading = 0,
    Ready = 1,
    Playing = 2,
    Paused = 3,
}

impl MwMusicState {
    /// `mw_core::MusicState` から変換する(判別子は常に一致する。往復不要な
    /// 一方向変換なので `from_raw` は用意しない)。
    pub fn from_core(state: mw_core::MusicState) -> Self {
        match state {
            mw_core::MusicState::Loading => MwMusicState::Loading,
            mw_core::MusicState::Ready => MwMusicState::Ready,
            mw_core::MusicState::Playing => MwMusicState::Playing,
            mw_core::MusicState::Paused => MwMusicState::Paused,
        }
    }
}

/// `mw_music_get_position` が書き込む音楽クロックのスナップショット
/// (初期構築仕様『§4.4 音楽クロック』『§5.5』)。blittable(`#[repr(C)]`)。
///
/// フィールドは `mw_core::MusicClockSnapshot` に1:1で対応する。毎フレーム呼ばれる
/// 関数の出力型(初期構築仕様『§5.4』: GC アロケーションゼロ)なので、Marshal を
/// 挟まず直接読める構造にしてある(`event.rs::MwEvent` と同じ設計方針。あちらの
/// ドキュメントコメント「payload を種別ごとに分けず1本にまとめた理由」も参照)。
///
/// ## `is_playing` を `bool` ではなく `u8` にした理由
///
/// C# の `bool` は既定のマーシャリングでは 4 バイト(Win32 `BOOL` 相当)として
/// 扱われることがあり、Rust の `bool`(`#[repr(C)]` 構造体内でも常に 1 バイト)と
/// 単純な `[StructLayout(LayoutKind.Sequential)]` ではレイアウトが一致しない
/// (フィールドごとに `[MarshalAs(UnmanagedType.U1)]` を明示すれば一致させられるが、
/// csbindgen の自動生成コードにその指定をさせる仕組みは無く、手で後から
/// 付け足すのは「手書きの宣言ズレを構造的に排除する」という csbindgen 採用の目的
/// (`crates/mw-ffi/CLAUDE.md`)に反する)。そこで `event.rs::MwEvent` の
/// `payload: u64` と同じ考え方で、素の `u8`(0 または 1)として持たせ、C# 側の
/// bool への解釈は呼び出し側(手書きの薄いラッパ `Mw.Native`)の責務にする——
/// これで構造体全体が疑いなく blittable になる。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MwMusicPosition {
    /// 楽曲ボイスをこれまでにレンダリングしたフレーム数(= 曲位置)。
    pub song_frames: u64,
    /// 上記フレーム数に対応するホスト単調時刻(ナノ秒)。`mw_host_time_ns()` と
    /// 同じ時計。
    pub host_time_ns: u64,
    /// 出力サンプルレート [Hz]。0 は「まだ確定していない」。
    pub sample_rate: u32,
    /// 楽曲ボイスの再生状態。`u8`/`i32` ではなく [`MwMusicState`] を直接フィールド型に
    /// している理由は [`MwMusicState`] のドキュメント参照(Rust が書き込む値は常に
    /// 妥当な判別子であり、C# が不正値を書き戻す経路が無いため enum を直接使っても
    /// 未定義動作の余地が無い。かつ csbindgen に C# `enum` を生成させる実際の
    /// 仕掛けにもなっている)。
    pub state: MwMusicState,
    /// 楽曲が進行中か(`0`/`1`)。上記「`bool` ではなく `u8` にした理由」参照。
    pub is_playing: u8,
    /// 世代カウンタ。不連続(シーク・停止・巻き戻し再開・ループ折り返し)のたびに
    /// 1 増える。C# 側は世代を跨いだ外挿・補間をしてはならない
    /// (`mw_core::MusicClockSnapshot` のドキュメント参照)。
    pub generation: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_state_from_core_matches_the_shared_discriminant_order() {
        assert_eq!(
            MwMusicState::from_core(mw_core::MusicState::Loading) as i32,
            mw_core::MusicState::Loading.to_u8() as i32
        );
        assert_eq!(
            MwMusicState::from_core(mw_core::MusicState::Ready) as i32,
            mw_core::MusicState::Ready.to_u8() as i32
        );
        assert_eq!(
            MwMusicState::from_core(mw_core::MusicState::Playing) as i32,
            mw_core::MusicState::Playing.to_u8() as i32
        );
        assert_eq!(
            MwMusicState::from_core(mw_core::MusicState::Paused) as i32,
            mw_core::MusicState::Paused.to_u8() as i32
        );
    }

    /// blittable であること(単純な整数フィールドの並びであること)の直接的な確認
    /// (`event.rs::mw_event_size_is_16_bytes` と同じ流儀)。8/8/4/4/1/4 バイトの
    /// フィールド列は `is_playing`(1バイト)の直後に3バイトのパディングが入り、
    /// 8バイト境界に揃って32バイトになる。
    #[test]
    fn mw_music_position_size_is_32_bytes() {
        assert_eq!(std::mem::size_of::<MwMusicPosition>(), 32);
    }

    #[test]
    fn sound_mode_from_raw_round_trips_and_rejects_unknown() {
        assert_eq!(MwSoundMode::from_raw(0), Some(MwSoundMode::Se));
        assert_eq!(MwSoundMode::from_raw(1), Some(MwSoundMode::Music));
        assert_eq!(MwSoundMode::from_raw(2), None);
        assert_eq!(MwSoundMode::from_raw(-1), None);
    }

    #[test]
    fn bus_from_raw_round_trips_and_rejects_unknown() {
        assert_eq!(MwBus::from_raw(0), Some(MwBus::Master));
        assert_eq!(MwBus::from_raw(3), Some(MwBus::Voice));
        assert_eq!(MwBus::from_raw(4), None);
    }

    #[test]
    fn bus_to_core_matches_discriminant() {
        assert_eq!(MwBus::Master.to_core() as u8, 0);
        assert_eq!(MwBus::Voice.to_core() as u8, 3);
    }
}
