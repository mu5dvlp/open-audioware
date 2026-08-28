//! C ABI 境界の blittable イベント表現(初期構築仕様『§4.6 イベント通知』, M2-6)。
//!
//! `mw_poll_events`(`ffi.rs`)がこの型を直接呼び出し側バッファへ書き込む。
//! csbindgen の入力(`build.rs` がこのファイルも読む)。

/// イベント種別。値は csbindgen が生成する C# 側 `enum MwEventKind : int` と一致する。
///
/// `RouteChanged`(初期構築仕様『§6 テンプレートとの連携ポイント』)は iOS / tvOS で
/// `crates/mw-backend/src/ios_interruption.rs` が発火させる(M3。reason を問わず
/// ルート変化のたびに1回。詳細は `mw_core::Event::RouteChanged` のドキュメント参照)。
/// Android では未配線。
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MwEventKind {
    RouteChanged = 0,
    Underrun = 1,
    MusicEnded = 2,
    MusicLooped = 3,
    StreamError = 4,
    /// 開発ビルドのみ発火(`mw_core::mixer::Mixer::render` 側の判定。判別子自体は
    /// リリースビルドでも同じ値のまま予約しておく——ビルド構成によって列挙値が
    /// ずれると、生成される C# バインディングの意味がビルドごとに変わってしまうため)。
    ClipperEngaged = 5,
    /// OS 主導のオーディオ割り込みが始まった(M3)。`mw_core::Event::AudioInterruptionBegan`
    /// のドキュメント参照。iOS 実機で「バックグラウンドから復帰すると SE だけ無音になる」
    /// 問題の観測手段として追加した(`crates/mw-backend/src/ios_interruption.rs`)。
    AudioInterruptionBegan = 6,
    /// 割り込みが終わった/復帰を試みた(M3)。`payload` は
    /// `mw_core::Event::AudioInterruptionEnded::recovered`(0 or 1)をそのまま `u64` 化。
    AudioInterruptionEnded = 7,
}

/// C# へポーリングで渡す1件分のイベント(blittable, `#[repr(C)]`)。
///
/// Marshal を挟まず直接読める形にするため、可変長データ・参照型は一切持たない
/// (初期構築仕様『§4.6』「呼び出し側が確保したバッファに blittable 構造体で書き込み、
/// GC アロケーションゼロ」)。
///
/// # `payload` を種別ごとに分けず1本(u64)にまとめた理由
///
/// 種別ごとに付随データの意味は異なる(`Underrun` はフレーム数、`MusicLooped` は
/// 折り返し位置、`StreamError` は理由コード)が、**いずれも 64bit 符号無し整数
/// 1個に収まる**。選択肢として C の `union` 相当(Rust の `#[repr(C)] union` や
/// 複数フィールドを持つ構造体)も検討したが、
///
/// - union は C# 側で `[StructLayout(LayoutKind.Explicit)]` の `FieldOffset` 越しに
///   読む必要があり、「Marshal を挟まず直接読める」という要件に対してかえって
///   C# 側の可読性を落とす(どのフィールドを読むべきかを `kind` を見て呼び出し側が
///   毎回判断する点は `payload` 1本でも union でも変わらない)
/// - 現状のどの種別も付随データは高々1個の数値で表現でき、複数フィールドを
///   同時に持つ種別が無い(将来増えた場合は `payload` を複数本に増やすか、
///   その時点で union 化を検討すればよい——ABI 破壊は semver メジャーでのみ
///   許可される、初期構築仕様『§4.8』)
///
/// という理由から、最も単純な「タグ + 単一の数値」で足りると判断した。
///
/// 各種別での意味(`kind` ごと。`RouteChanged`/`MusicEnded`/`ClipperEngaged` は
/// 付随データを持たないため常に 0):
/// - `Underrun`: このコールバック群で集約されたアンダーランフレーム数
///   (`mw_core::mixer::Mixer::report_underrun` のドキュメント参照。1コールバックごと
///   ではなく、連続するアンダーランをまとめて報告する)
/// - `MusicLooped`: 折り返し先(曲頭からのフレーム位置)
/// - `StreamError`: `mw_core::StreamErrorReason` の判別子をそのまま `u64`化したもの
/// - `AudioInterruptionEnded`: 復帰(ストリーム再開)を試みて成功したら 1、それ以外は 0
///   (`AudioInterruptionBegan` は付随データを持たないため常に 0)
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MwEvent {
    pub kind: MwEventKind,
    pub payload: u64,
}

impl MwEvent {
    /// mw-core 側の内部表現(`mw_core::Event`)から FFI 境界の blittable 表現へ変換する。
    pub(crate) fn from_core(event: mw_core::Event) -> Self {
        match event {
            mw_core::Event::RouteChanged => Self {
                kind: MwEventKind::RouteChanged,
                payload: 0,
            },
            mw_core::Event::Underrun { frames } => Self {
                kind: MwEventKind::Underrun,
                payload: frames as u64,
            },
            mw_core::Event::MusicEnded => Self {
                kind: MwEventKind::MusicEnded,
                payload: 0,
            },
            mw_core::Event::MusicLooped { restart_frame } => Self {
                kind: MwEventKind::MusicLooped,
                payload: restart_frame,
            },
            mw_core::Event::StreamError { reason } => Self {
                kind: MwEventKind::StreamError,
                payload: reason as u64,
            },
            mw_core::Event::ClipperEngaged => Self {
                kind: MwEventKind::ClipperEngaged,
                payload: 0,
            },
            mw_core::Event::AudioInterruptionBegan => Self {
                kind: MwEventKind::AudioInterruptionBegan,
                payload: 0,
            },
            mw_core::Event::AudioInterruptionEnded { recovered } => Self {
                kind: MwEventKind::AudioInterruptionEnded,
                payload: recovered as u64,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_core_maps_each_variant_to_the_expected_kind_and_payload() {
        assert_eq!(
            MwEvent::from_core(mw_core::Event::RouteChanged),
            MwEvent {
                kind: MwEventKind::RouteChanged,
                payload: 0
            }
        );
        assert_eq!(
            MwEvent::from_core(mw_core::Event::Underrun { frames: 42 }),
            MwEvent {
                kind: MwEventKind::Underrun,
                payload: 42
            }
        );
        assert_eq!(
            MwEvent::from_core(mw_core::Event::MusicEnded),
            MwEvent {
                kind: MwEventKind::MusicEnded,
                payload: 0
            }
        );
        assert_eq!(
            MwEvent::from_core(mw_core::Event::MusicLooped { restart_frame: 999 }),
            MwEvent {
                kind: MwEventKind::MusicLooped,
                payload: 999
            }
        );
        assert_eq!(
            MwEvent::from_core(mw_core::Event::StreamError {
                reason: mw_core::StreamErrorReason::DeviceUnavailable
            }),
            MwEvent {
                kind: MwEventKind::StreamError,
                payload: mw_core::StreamErrorReason::DeviceUnavailable as u64
            }
        );
        assert_eq!(
            MwEvent::from_core(mw_core::Event::ClipperEngaged),
            MwEvent {
                kind: MwEventKind::ClipperEngaged,
                payload: 0
            }
        );
        assert_eq!(
            MwEvent::from_core(mw_core::Event::AudioInterruptionBegan),
            MwEvent {
                kind: MwEventKind::AudioInterruptionBegan,
                payload: 0
            }
        );
        assert_eq!(
            MwEvent::from_core(mw_core::Event::AudioInterruptionEnded { recovered: true }),
            MwEvent {
                kind: MwEventKind::AudioInterruptionEnded,
                payload: 1
            }
        );
        assert_eq!(
            MwEvent::from_core(mw_core::Event::AudioInterruptionEnded { recovered: false }),
            MwEvent {
                kind: MwEventKind::AudioInterruptionEnded,
                payload: 0
            }
        );
    }

    /// blittable であること(=単純な整数フィールドの並びであること)の直接的な確認。
    /// `#[repr(C)]` + フィールドの並びが変わらない限り、C# の
    /// `[StructLayout(LayoutKind.Sequential)]` と一致するレイアウトになる
    /// (`kind` を i32 の4バイト、`payload` を u64 の8バイト境界に揃えるためのパディング
    /// 4バイトを挟み、合計16バイトになる——C 側の自然な構造体レイアウトと一致する)。
    #[test]
    fn mw_event_size_is_16_bytes() {
        assert_eq!(std::mem::size_of::<MwEvent>(), 16);
    }
}
