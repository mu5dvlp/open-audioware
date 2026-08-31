//! `cargo test` から C# 側の手書きラッパ(`unity/Runtime/MwNative.cs`)と Rust 側の
//! FFI 境界の値を突き合わせ、ズレを機械的に検出するテスト専用モジュール。
//!
//! # 背景(なぜこのモジュールがあるか)
//!
//! `unity/Runtime/Generated/NativeMethods.g.cs` は csbindgen が `crates/mw-ffi/src/`
//! (`ffi.rs`/`result.rs`/`event.rs`/`types.rs`)から自動生成するため、**そちらは
//! 構造的にズレようがない**(`build.rs` が毎回上書きする。手書きの宣言ズレを
//! 構造的に排除するのが csbindgen 採用の目的そのもの)。
//!
//! ズレが起きるのは、生成物を直接公開せず薄いラッパを挟んでいる箇所——
//! `unity/Runtime/MwNative.cs` が独自に持つ**公開用の列挙**(`MwResult` /
//! `SoundMode` / `Bus` / `MusicState` / `EventKind` / `StreamErrorReason`)。
//! これらは「生成コードの `internal` 型をパッケージ外へ漏らさない」ために手で
//! コピーされた値であり、Rust 側の判別子が変わっても C# 側は**自動的には**
//! 追従しない。実際に `AudioInterruptionBegan`/`AudioInterruptionEnded`
//! (値 6/7)が M3 以降ずっと C# 側に欠けていた(`docs/history/05-2026-08-31.md`)。
//!
//! `StreamErrorReason` はさらに事情が違う: 定義そのものが `mw_core`(`mw-ffi` の
//! csbindgen 入力に含まれないクレート)にあり、`extern "C"` 関数のシグネチャにも
//! 構造体のフィールド型にも直接現れない(常に `u64` へ変換済みの値としてしか
//! FFI 境界を越えない)ため、**csbindgen で自動生成する経路が原理的に存在しない**。
//! したがって C# 側の対応する列挙は必ず手書きになり、今後もこのテストのような
//! 手動同期の検証が要る。
//!
//! # 設計: 二重の防御
//!
//! 1. **コンパイル時**: 各 `describe_*` 関数の中で Rust の enum を **ワイルドカード
//!    無しの `match`** に通す。Rust 側に新しい判別子(variant)が増えると、この
//!    `match` が非網羅になり **`mw-ffi` クレート自体がコンパイルできなくなる**。
//!    つまり「新しい判別子を追加したのに誰もこのファイルへ触れない」という
//!    事態がそもそも起こり得ない——追加した本人が必ずここへ来て、対応する
//!    C# 側の値も一緒に確認する動線になる。
//! 2. **実行時**(`cargo test`): 各 `describe_*` が返す「(名前, 判別子)の全件」を、
//!    `unity/Runtime/MwNative.cs` を `include_str!` で読み込んでテキストとして
//!    抽出した C# 側の値と突き合わせる。名前の集合(過不足)と値の両方を検査するため、
//!    「C# 側に新しい判別子が無い」「値がズレている」の両方を検出する。
//!
//! # なぜ `unity-sample` の EditMode テストではなくこの形にしたか
//!
//! `unity-sample` の EditMode テストは Unity の起動が要るため CI では回らない方針
//! (`CLAUDE.md`)。このモジュールは `cargo test --workspace` だけで完結し、
//! Unity にもネットワークにも依存しない(`include_str!` はコンパイル時にソースへ
//! 埋め込まれるため、実行時のファイル探索も不要)。

use std::collections::BTreeMap;

use crate::event::MwEventKind;
use crate::result::MwResult;
use crate::types::{MwBus, MwMusicState, MwSoundMode};

/// `unity/Runtime/MwNative.cs`(手書きラッパ)のソース全体。コンパイル時に埋め込む
/// (`crates/mw-ffi/src` から見て3階層上がリポジトリルート)。
const MW_NATIVE_CS: &str = include_str!("../../../unity/Runtime/MwNative.cs");

/// C# の `public enum <name> { ... }` ブロックから `名前 = 値` の対応表を抽出する。
///
/// XML doc コメント(`///` 行)は事前に除外してから走査するため、コメント中に
/// 偶然 `Foo = 1` のような文字列が現れても誤検出しない。
///
/// 見つからない場合は panic する(fail-closed): 「列挙が C# 側に無い」ことも
/// このテストが検出すべきズレの一種なので、黙ってスキップしない。
fn extract_csharp_enum(source: &str, name: &str) -> BTreeMap<String, i64> {
    let marker = format!("enum {name}");
    let decl_start = source.find(&marker).unwrap_or_else(|| {
        panic!(
            "unity/Runtime/MwNative.cs に `enum {name}` が見つからない。\
             Rust 側に対応する型があるのに C# 側の手書きミラーが無い(削除された/\
             まだ追加していない)可能性がある。"
        )
    });
    let body_start = source[decl_start..]
        .find('{')
        .map(|i| decl_start + i + 1)
        .unwrap_or_else(|| panic!("`enum {name}` の開き波括弧が見つからない"));
    let body_end = source[body_start..]
        .find('}')
        .map(|i| body_start + i)
        .unwrap_or_else(|| panic!("`enum {name}` の閉じ波括弧が見つからない"));
    let body = &source[body_start..body_end];

    let mut values = BTreeMap::new();
    let mut next_implicit: i64 = 0;
    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("///") || line.starts_with("//") {
            continue;
        }
        let line = line.trim_end_matches(',').trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, '=');
        let member_name = parts.next().unwrap().trim();
        if member_name.is_empty() {
            continue;
        }
        let value = match parts.next() {
            Some(raw_value) => raw_value.trim().parse::<i64>().unwrap_or_else(|_| {
                panic!(
                    "`enum {name}` のメンバ `{member_name}` の値をパースできない: \
                     {raw_line:?}"
                )
            }),
            None => next_implicit,
        };
        next_implicit = value + 1;
        values.insert(member_name.to_string(), value);
    }
    values
}

/// Rust 側の判別子一覧と C# 側の判別子一覧を突き合わせ、名前の過不足・値のズレを
/// まとめて報告する(1回の assert で全差分が見えるよう、最初の不一致で即 panic
/// せず全件集めてからまとめて落とす)。
fn assert_enums_in_sync(rust_enum_name: &str, csharp_enum_name: &str, rust: &[(&str, i64)]) {
    let rust_map: BTreeMap<String, i64> = rust
        .iter()
        .map(|(name, value)| (name.to_string(), *value))
        .collect();
    let csharp_map = extract_csharp_enum(MW_NATIVE_CS, csharp_enum_name);

    let mut problems = Vec::new();

    for (name, rust_value) in &rust_map {
        match csharp_map.get(name) {
            None => problems.push(format!(
                "C# `{csharp_enum_name}` に `{name}` が無い(Rust `{rust_enum_name}::{name}` \
                 = {rust_value})"
            )),
            Some(csharp_value) if csharp_value != rust_value => problems.push(format!(
                "`{name}` の値がズレている: Rust `{rust_enum_name}::{name}` = {rust_value}, \
                 C# `{csharp_enum_name}.{name}` = {csharp_value}"
            )),
            Some(_) => {}
        }
    }
    for name in csharp_map.keys() {
        if !rust_map.contains_key(name) {
            problems.push(format!(
                "C# `{csharp_enum_name}.{name}` に対応する Rust `{rust_enum_name}` の判別子が無い\
                 (C# 側だけにある不要な値、または命名がズレている)"
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "Rust `{rust_enum_name}` と C# `{csharp_enum_name}` がズレている:\n  - {}",
        problems.join("\n  - ")
    );
}

/// `MwResult` の全判別子を列挙する。
///
/// この `match` はワイルドカード無し: `MwResult` に新しい判別子を追加すると、
/// ここが非網羅になり `mw-ffi` クレートのコンパイルが失敗する
/// (モジュール doc「設計: 二重の防御」参照)。
fn describe_mw_result(v: MwResult) -> &'static str {
    match v {
        MwResult::Ok => "Ok",
        MwResult::ErrNullPointer => "ErrNullPointer",
        MwResult::ErrInvalidHandle => "ErrInvalidHandle",
        MwResult::ErrBackendOpenFailed => "ErrBackendOpenFailed",
        MwResult::ErrBackendCloseFailed => "ErrBackendCloseFailed",
        MwResult::ErrPanic => "ErrPanic",
        MwResult::ErrUnsupportedSoundMode => "ErrUnsupportedSoundMode",
        MwResult::ErrDecodeFailed => "ErrDecodeFailed",
        MwResult::ErrUnsupportedSampleRate => "ErrUnsupportedSampleRate",
        MwResult::ErrUnsupportedFormat => "ErrUnsupportedFormat",
        MwResult::ErrInvalidSoundId => "ErrInvalidSoundId",
        MwResult::ErrCommandQueueFull => "ErrCommandQueueFull",
        MwResult::ErrInvalidBus => "ErrInvalidBus",
        MwResult::ErrInvalidLoopRegion => "ErrInvalidLoopRegion",
    }
}

const ALL_MW_RESULT: [MwResult; 14] = [
    MwResult::Ok,
    MwResult::ErrNullPointer,
    MwResult::ErrInvalidHandle,
    MwResult::ErrBackendOpenFailed,
    MwResult::ErrBackendCloseFailed,
    MwResult::ErrPanic,
    MwResult::ErrUnsupportedSoundMode,
    MwResult::ErrDecodeFailed,
    MwResult::ErrUnsupportedSampleRate,
    MwResult::ErrUnsupportedFormat,
    MwResult::ErrInvalidSoundId,
    MwResult::ErrCommandQueueFull,
    MwResult::ErrInvalidBus,
    MwResult::ErrInvalidLoopRegion,
];

/// `MwSoundMode` の全判別子。ワイルドカード無し `match`(理由は上記参照)。
fn describe_mw_sound_mode(v: MwSoundMode) -> &'static str {
    match v {
        MwSoundMode::Se => "Se",
        MwSoundMode::Music => "Music",
    }
}

const ALL_MW_SOUND_MODE: [MwSoundMode; 2] = [MwSoundMode::Se, MwSoundMode::Music];

/// `MwBus` の全判別子。ワイルドカード無し `match`(理由は上記参照)。
fn describe_mw_bus(v: MwBus) -> &'static str {
    match v {
        MwBus::Master => "Master",
        MwBus::Bgm => "Bgm",
        MwBus::Se => "Se",
        MwBus::Voice => "Voice",
    }
}

const ALL_MW_BUS: [MwBus; 4] = [MwBus::Master, MwBus::Bgm, MwBus::Se, MwBus::Voice];

/// `MwMusicState` の全判別子。ワイルドカード無し `match`(理由は上記参照)。
fn describe_mw_music_state(v: MwMusicState) -> &'static str {
    match v {
        MwMusicState::Loading => "Loading",
        MwMusicState::Ready => "Ready",
        MwMusicState::Playing => "Playing",
        MwMusicState::Paused => "Paused",
    }
}

const ALL_MW_MUSIC_STATE: [MwMusicState; 4] = [
    MwMusicState::Loading,
    MwMusicState::Ready,
    MwMusicState::Playing,
    MwMusicState::Paused,
];

/// `MwEventKind` の全判別子。ワイルドカード無し `match`(理由は上記参照)。
///
/// **背景の再発防止テスト**: `AudioInterruptionBegan`/`AudioInterruptionEnded` が
/// C# 側に欠けていた実際のバグ(`docs/history/05-2026-08-31.md`)は、ここに
/// ワイルドカード無し `match` があれば「Rust に足したのに C# 側を触っていない」
/// 段階で `mw-ffi` のコンパイルが失敗し検出できていたはずのケース。
fn describe_mw_event_kind(v: MwEventKind) -> &'static str {
    match v {
        MwEventKind::RouteChanged => "RouteChanged",
        MwEventKind::Underrun => "Underrun",
        MwEventKind::MusicEnded => "MusicEnded",
        MwEventKind::MusicLooped => "MusicLooped",
        MwEventKind::StreamError => "StreamError",
        MwEventKind::ClipperEngaged => "ClipperEngaged",
        MwEventKind::AudioInterruptionBegan => "AudioInterruptionBegan",
        MwEventKind::AudioInterruptionEnded => "AudioInterruptionEnded",
    }
}

const ALL_MW_EVENT_KIND: [MwEventKind; 8] = [
    MwEventKind::RouteChanged,
    MwEventKind::Underrun,
    MwEventKind::MusicEnded,
    MwEventKind::MusicLooped,
    MwEventKind::StreamError,
    MwEventKind::ClipperEngaged,
    MwEventKind::AudioInterruptionBegan,
    MwEventKind::AudioInterruptionEnded,
];

/// `mw_core::StreamErrorReason` の全判別子。ワイルドカード無し `match`(理由は上記参照)。
///
/// この型は csbindgen の入力に含まれない(モジュール doc 参照)ため、C# 側は
/// 完全に手書き——このテストが唯一の機械的な同期チェック。
fn describe_stream_error_reason(v: mw_core::StreamErrorReason) -> &'static str {
    match v {
        mw_core::StreamErrorReason::Unknown => "Unknown",
        mw_core::StreamErrorReason::DeviceUnavailable => "DeviceUnavailable",
        mw_core::StreamErrorReason::Reconfigured => "Reconfigured",
        mw_core::StreamErrorReason::PermissionDenied => "PermissionDenied",
        mw_core::StreamErrorReason::Backend => "Backend",
    }
}

const ALL_STREAM_ERROR_REASON: [mw_core::StreamErrorReason; 5] = [
    mw_core::StreamErrorReason::Unknown,
    mw_core::StreamErrorReason::DeviceUnavailable,
    mw_core::StreamErrorReason::Reconfigured,
    mw_core::StreamErrorReason::PermissionDenied,
    mw_core::StreamErrorReason::Backend,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mw_result_matches_csharp_mw_result() {
        let rust: Vec<(&str, i64)> = ALL_MW_RESULT
            .iter()
            .map(|&v| (describe_mw_result(v), v as i64))
            .collect();
        assert_enums_in_sync("MwResult", "MwResult", &rust);
    }

    #[test]
    fn mw_sound_mode_matches_csharp_sound_mode() {
        let rust: Vec<(&str, i64)> = ALL_MW_SOUND_MODE
            .iter()
            .map(|&v| (describe_mw_sound_mode(v), v as i64))
            .collect();
        assert_enums_in_sync("MwSoundMode", "SoundMode", &rust);
    }

    #[test]
    fn mw_bus_matches_csharp_bus() {
        let rust: Vec<(&str, i64)> = ALL_MW_BUS
            .iter()
            .map(|&v| (describe_mw_bus(v), v as i64))
            .collect();
        assert_enums_in_sync("MwBus", "Bus", &rust);
    }

    #[test]
    fn mw_music_state_matches_csharp_music_state() {
        let rust: Vec<(&str, i64)> = ALL_MW_MUSIC_STATE
            .iter()
            .map(|&v| (describe_mw_music_state(v), v as i64))
            .collect();
        assert_enums_in_sync("MwMusicState", "MusicState", &rust);
    }

    /// 直接の再発防止テスト: `AudioInterruptionBegan`/`AudioInterruptionEnded` が
    /// C# 側に欠けていたバグ(`docs/history/05-2026-08-31.md`)を、Unity を起動せず
    /// `cargo test` だけで検出できることを固定化する。
    #[test]
    fn mw_event_kind_matches_csharp_event_kind() {
        let rust: Vec<(&str, i64)> = ALL_MW_EVENT_KIND
            .iter()
            .map(|&v| (describe_mw_event_kind(v), v as i64))
            .collect();
        assert_enums_in_sync("MwEventKind", "EventKind", &rust);
    }

    #[test]
    fn stream_error_reason_matches_csharp_stream_error_reason() {
        let rust: Vec<(&str, i64)> = ALL_STREAM_ERROR_REASON
            .iter()
            .map(|&v| (describe_stream_error_reason(v), v as i64))
            .collect();
        assert_enums_in_sync("StreamErrorReason", "StreamErrorReason", &rust);
    }

    /// ABI バージョン定数(`mw_abi_version()` の戻り値)と C# 側の
    /// `MwNative.ExpectedAbiVersion` の同期を確認する。
    #[test]
    fn abi_version_matches_csharp_expected_abi_version() {
        let rust_version = crate::ffi::mw_abi_version();

        let marker = "ExpectedAbiVersion = ";
        let start = MW_NATIVE_CS.find(marker).unwrap_or_else(|| {
            panic!("unity/Runtime/MwNative.cs に `ExpectedAbiVersion = ` が見つからない")
        }) + marker.len();
        let rest = &MW_NATIVE_CS[start..];
        let end = rest
            .find(';')
            .unwrap_or_else(|| panic!("`ExpectedAbiVersion` の代入文の末尾(`;`)が見つからない"));
        let csharp_version: u32 = rest[..end].trim().parse().unwrap_or_else(|_| {
            panic!(
                "`ExpectedAbiVersion` の値をパースできない: {:?}",
                &rest[..end]
            )
        });

        assert_eq!(
            rust_version, csharp_version,
            "mw_abi_version() = {rust_version} だが C# `MwNative.ExpectedAbiVersion` = \
             {csharp_version}。ABI 互換の破壊は semver メジャーバージョンでのみ許可される \
             (初期構築仕様 §4.8)——両方を確認して更新すること。"
        );
    }

    /// `extract_csharp_enum` 自体の健全性(このテストファイルが壊れていないこと)。
    /// 存在しない列挙名を渡すと panic することを確認する——「見つからなければ
    /// 黙ってスキップ」ではなく fail-closed であることの直接の固定化。
    #[test]
    #[should_panic(expected = "が見つからない")]
    fn extract_csharp_enum_panics_when_the_enum_is_missing() {
        extract_csharp_enum(MW_NATIVE_CS, "ThisEnumDoesNotExistInMwNativeCs");
    }
}
