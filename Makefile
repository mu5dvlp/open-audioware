\
# open-audioware — 初期構築仕様 §7.3
#
# ターゲット一覧は `make help` で表示する。

.DEFAULT_GOAL := help

# --- 設定 -------------------------------------------------------------
# Unity Editor 本体・同梱 NDK のパス。CI やローカル環境に合わせて上書き可能。
UNITY_VERSION       ?= 6000.4.1f1
UNITY_HUB_EDITOR_DIR ?= /Applications/Unity/Hub/Editor/$(UNITY_VERSION)
UNITY_APP            ?= $(UNITY_HUB_EDITOR_DIR)/Unity.app/Contents/MacOS/Unity
ANDROID_NDK_HOME     ?= $(UNITY_HUB_EDITOR_DIR)/PlaybackEngines/AndroidPlayer/NDK
export ANDROID_NDK_HOME

# AAudio(Android のローレイテンシ出力 API)は API level 26 以降が必要。
ANDROID_API_LEVEL   ?= 26
ANDROID_ABI          ?= arm64-v8a

UNITY_SAMPLE_DIR     := unity-sample
UNITY_LOCK_RUNNER    := tools/with-unity-lock.sh

PLUGINS_MACOS_DIR    := unity/Runtime/Plugins/macOS
PLUGINS_IOS_DIR       := unity/Runtime/Plugins/iOS
PLUGINS_ANDROID_DIR   := unity/Runtime/Plugins/Android/libs/$(ANDROID_ABI)

XCFRAMEWORK           := $(PLUGINS_IOS_DIR)/MwFfi.xcframework

.PHONY: help setup lint format gitleaks test bench bindgen csharp-check \
        build-macos build-ios build-android \
        package unity-sample-create unity-test \
        measurement-scene measurement-export-ios measurement-build-android clean \
        miri miri-all miri-toolchain \
        fuzz-toolchain fuzz-build fuzz-corpus fuzz-run fuzz-seeds \
        tsan

help:
	@echo "open-audioware — make ターゲット"
	@echo "  make setup          - ツールチェーン・ターゲット・cargo-ndk 等の導入確認"
	@echo "  make lint           - fmt --check + clippy -D warnings + cargo-deny + 第三者表記の鮮度検査 + C# ラッパのコンパイル"
	@echo "  make csharp-check   - unity/Runtime/MwNative.cs を Unity 無しでコンパイル(P3-8)"
	@echo "  make third-party-licenses       - THIRD-PARTY-LICENSES.md を再生成(依存を足したら必ず実行)"
	@echo "  make third-party-licenses-check - 再生成して差分が無いか検査(CI 用)"
	@echo "  make format         - cargo fmt (自動整形)"
	@echo "  make test           - cargo test --workspace"
	@echo "  make miri           - unsafe を持つ ring_buffer と、パーサ(wav / decode / stream)のテストを Miri で実行(約2分。CI と同じ範囲)"
	@echo "  make miri-all       - mw-core の lib の全テストを Miri で実行(約13分。rubato の FFT と seqlock の並行テストは対象外)"
	@echo "  make fuzz-seeds     - fuzz/corpus/ のシードコーパスを再生成(fuzz/seeds/make_seeds.py)"
	@echo "  make fuzz-build     - cargo-fuzz のターゲット3本をビルド(wav_decode / wav_decoder_pump / resample)"
	@echo "  make fuzz-corpus    - 既存コーパスの回帰実行のみ(数秒。CI が呼ぶのはこれ)"
	@echo "  make fuzz-run       - TARGET=<name> SECONDS=<秒数、既定600> で1本を指定時間だけ実行(手元/週次専用)"
	@echo "  make tsan           - ThreadSanitizer でワークスペースのテストを実行(手元専用。ADR-0003 の3段目)"
	@echo "  make bench          - criterion ベンチ(未導入。任意)"
	@echo "  make doc            - API リファレンス(rustdoc)を生成。docs/integration.md §6 が正とする出力"
	@echo "  make doc-coverage   - C ABI の全エクスポート関数に doc コメントがあるか検査"
	@echo "  make bindgen        - csbindgen で C# バインディング生成"
	@echo "  make build-macos    - universal .dylib をビルドし unity/Runtime/Plugins/macOS/ へ配置"
	@echo "  make build-ios      - aarch64-apple-ios 静的ライブラリ → xcframework"
	@echo "  make build-android  - cargo-ndk で $(ANDROID_ABI) の .so を生成"
	@echo "  make package        - UPM パッケージ組み立て(スタブ)"
	@echo "  make unity-sample-create - unity-sample/ プロジェクトを作成(初回のみ。要 Unity ロック)"
	@echo "  make unity-test     - unity-sample の EditMode テストを実行(要 Unity ロック)"
	@echo "  make measurement-scene     - A/B 計測シーンを生成/更新(要 Unity ロック)"
	@echo "  make measurement-export-ios - A/B 計測アプリの Xcode プロジェクトを書き出す(要 Unity ロック)"
	@echo "  make measurement-build-android - A/B 計測アプリの apk を書き出す(要 Unity ロック)"
	@echo "  make clean          - target/ 以下のビルド成果物を削除"

# --- setup --------------------------------------------------------------

setup:
	@echo "== rustup targets =="
	rustup target add \
		aarch64-apple-darwin \
		x86_64-apple-darwin \
		aarch64-apple-ios \
		aarch64-linux-android
	@echo "== cargo-ndk =="
	@if ! command -v cargo-ndk >/dev/null 2>&1; then \
		echo "cargo-ndk が見つからないため導入します"; \
		cargo install cargo-ndk; \
	else \
		echo "cargo-ndk は導入済み: $$(cargo ndk --version)"; \
	fi
	@echo "== cargo-deny =="
	@if ! command -v cargo-deny >/dev/null 2>&1; then \
		echo "cargo-deny が見つからないため導入します"; \
		cargo install cargo-deny; \
	else \
		echo "cargo-deny は導入済み: $$(cargo deny --version)"; \
	fi
	@echo "== ANDROID_NDK_HOME =="
	@if [ ! -d "$(ANDROID_NDK_HOME)" ]; then \
		echo "警告: ANDROID_NDK_HOME が見つかりません: $(ANDROID_NDK_HOME)"; \
		echo "  Unity Hub で Android Build Support (+ NDK) を導入するか、"; \
		echo "  ANDROID_NDK_HOME を明示的に指定してください。"; \
	else \
		echo "ANDROID_NDK_HOME: $(ANDROID_NDK_HOME)"; \
	fi

# --- lint / format / test ------------------------------------------------

lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo deny check
	@$(MAKE) --no-print-directory third-party-licenses-check
	@$(MAKE) --no-print-directory csharp-check

# ===========================================================================
# C# ラッパのコンパイル検査(P3-8)
# ===========================================================================
#
# 🔴 `unity/Runtime/MwNative.cs` は**手書き**で、csbindgen 生成物のフィールドを
# 名指しで詰め替えている。Rust 側でフィールド名・型・シグネチャが変わると
# **壊れるのは C# のコンパイルだけ**で、cargo test は全部緑のまま通る。
# Unity のテストは CI で回さない方針(コーヒー基準)なので、C# の ABI ずれを
# Unity 無しでも検出できるよう、この検査を独立して実行する。
#
# ⚠️ **レイアウト(オフセット・サイズ)はこちらでは見ていない。**
# そちらは `crates/mw-ffi/src/csharp_abi_sync.rs` の `offset_of!` assert が固定する
# (フィールドの並べ替えは C# のコンパイルを通ってしまう。実測で確認済み)。
# 理由と分担は `tools/csharp-abi-check/README.md`。
#
# ⚠️ **iOS の `__Internal` 分岐も別に1回コンパイルする。** 生成物の
# `#if UNITY_IOS && !UNITY_EDITOR` は既定ビルドでは通らず、ここは過去に実際の
# iOS リンク失敗を生んだ箇所(`crates/mw-ffi/build.rs` のコメント参照)。
CSHARP_ABI_PROJECT := tools/csharp-abi-check/MwAbiCheck.csproj
CSHARP_GENERATED := unity/Runtime/Generated/NativeMethods.g.cs

csharp-check: ## MwNative.cs を Unity 無しでコンパイルする(P3-8)
	@command -v dotnet >/dev/null 2>&1 || { \
	  echo "[error] dotnet が見つかりません。C# ラッパのコンパイル検査ができないため中断します。"; \
	  echo "        インストール: https://dotnet.microsoft.com/download (SDK 8.0 以降)"; \
	  echo "        ⚠️ 未インストールを黙って skip すると『嘘の緑』になるため、あえて失敗させています。"; \
	  exit 1; \
	}
	@test -f $(CSHARP_GENERATED) || { \
	  echo "[info] $(CSHARP_GENERATED) が無いので cargo build で生成します"; \
	  cargo build -p mw-ffi; \
	}
	@echo "[csharp-check] 既定(Unity Editor / macOS / Android 経路)"
	@dotnet build $(CSHARP_ABI_PROJECT) -v q --nologo
	@echo "[csharp-check] UNITY_IOS(__Internal 静的リンク経路)"
	@dotnet build $(CSHARP_ABI_PROJECT) -v q --nologo \
	  -p:DefineConstants=UNITY_IOS \
	  -p:BaseOutputPath=$(CURDIR)/tools/csharp-abi-check/obj/out-ios/
	@echo "[ok] C# ラッパは両方の分岐でコンパイルできました"

# ===========================================================================
# gitleaks(秘密情報のコミット検知)
# ===========================================================================

# 🔴 **バージョンはここ1箇所だけで固定する。** CI もこのターゲットを呼ぶので、
# 「CI と Makefile で同じ検査を二重に書いて手で揃える」形を作らない。
# ⚠️ ローカルに gitleaks のバイナリが入っている場合はそちらが使われるため、
# バージョン差で結果が変わりうる —— 実行時に必ずどちらを使ったかを表示する。
#
# ⚠️ **lint には含めない。** lint は毎回の内側ループで回すもので、gitleaks は git 履歴を
# 丸ごと読む(このリポジトリで約3秒、client では約25秒)。目的も頻度も違うので独立させる。
GITLEAKS_VERSION := v8.30.1
GITLEAKS_IMAGE := zricethezav/gitleaks:$(GITLEAKS_VERSION)

gitleaks: ## 秘密情報がコミットされていないか git 履歴ごと検査する
	@if command -v gitleaks >/dev/null 2>&1; then \
		echo "== gitleaks(ローカルのバイナリ: $$(gitleaks version)。CI は $(GITLEAKS_VERSION)) =="; \
		gitleaks git . --redact --no-banner; \
	elif command -v docker >/dev/null 2>&1; then \
		echo "== gitleaks（$(GITLEAKS_IMAGE)） =="; \
		docker run --rm -v "$(CURDIR):/repo" $(GITLEAKS_IMAGE) git /repo --redact --no-banner; \
	else \
		echo "[error] gitleaks も docker も見つかりません。どちらかを用意してください。"; \
		echo "        brew install gitleaks  # または Docker Desktop を起動する"; \
		exit 1; \
	fi

# --- ライセンス -----------------------------------------------------------

# open-audioware 自身は MIT-0(表記不要)だが、依存クレートの表記義務は消せない。
# 利用者が自分で調べなくて済むよう、こちらで 1 枚にまとめて同梱する。
# ⚠️ **依存を足したり消したりしたら必ず再生成すること。** `make lint` が鮮度を検査する。
third-party-licenses:
	@python3 scripts/gen-third-party-licenses.py

# 生成物が古いまま公開されるのを防ぐ。CI(= make lint)から呼ばれる。
# 🔴 差分が出たら `make third-party-licenses` を実行してコミットすること。
third-party-licenses-check:
	@tmp_file=$$(mktemp); \
	trap 'rm -f "$$tmp_file"' EXIT; \
	python3 scripts/gen-third-party-licenses.py --output "$$tmp_file" >/dev/null; \
	cmp -s "$$tmp_file" THIRD-PARTY-LICENSES.md || ( \
		echo "[error] THIRD-PARTY-LICENSES.md が依存構成と食い違っています。"; \
		echo "        make third-party-licenses を実行してください。"; \
		diff -u THIRD-PARTY-LICENSES.md "$$tmp_file" || true; \
		exit 1 )
	@echo "[ok] THIRD-PARTY-LICENSES.md は最新です"

format:
	cargo fmt --all

test:
	cargo test --workspace

# --- Miri(unsafe の未定義動作の検査)---------------------------------------
#
# 対象は mw-core だけ。自前のロックフリーリングバッファ(ring_buffer.rs)とデコーダは
# 外部クレートを置き換えた自前実装で、unsafe のポインタ操作を Miri 以外に検査する手段が無い。
# mw-backend / mw-ffi は cpal(C の音声デバイス)と FFI を呼ぶため Miri では動かせない。
#
# nightly は stable(rust-toolchain.toml)とは別に、ここで日付固定で持つ。
# 🔴 日付はこの変数(NIGHTLY_TOOLCHAIN)だけに書く(CI は `make miri` / `make fuzz-corpus` を
# 呼ぶだけで、日付を持たない)。上げるときは手元で `make miri` を通してから変える。
# Miri 専用だった名前の名残で `MIRI_TOOLCHAIN` も残す(cargo-fuzz(ADR-0003 の2段目)も
# 同じ nightly を共有するため、変数名を先に一般化した)。
NIGHTLY_TOOLCHAIN := nightly-2026-09-27
MIRI_TOOLCHAIN := $(NIGHTLY_TOOLCHAIN)
# Miri はホストに関係なくこのターゲットとして解釈する(rust-src から std をビルドするので
# クロスでも動く)。x86_64-apple-darwin は SSE4.1 が既定で有効なため rustfft(rubato の依存)が
# SSE 経路を選び、その中の `_mm_load1_pd` が 4 バイト整列の Complex<f32> を 8 バイト整列として
# 読む上流の未定義動作を Miri が止めてしまう。x86_64 Linux は SSE2 までなのでスカラー経路になり、
# 自分たちのコード(mw-core)の検査に集中できる。CI(ubuntu)とも同じ条件になる。
MIRI_TARGET := x86_64-unknown-linux-gnu

# `miri` は CI に載せる範囲: unsafe を持つ唯一のモジュール(ring_buffer)と、外部のバイト列を読む
# パーサ(wav / decode)、リングバッファへ書き込む経路(stream)。実測 40 件 109 秒(コーヒー基準内)。
# mixer / music などは音声を描画するため Miri では1件ずつ数十秒かかり、lib 全体では約13分になるので
# `miri-all` に分けて手元で回す(依存を自前化した直後や、unsafe を触ったときに1回)。
MIRI_CI_FILTER := ring_buffer wav decode stream
# リサンプラ自身のテスト(`resample::`。フィルタの `stream` に当たる)と、リサンプラを通して音声を
# 丸ごと変換するテストは Miri では1件1分前後かかり、CI の範囲が10分を超えるので CI では外し、
# `miri-all` で回す(リサンプラは unsafe を持たない)。
MIRI_CI_SKIP := \
	resample:: \
	resample_preserves_frequency_when_upsampling_44_1k_to_48k \
	resample_produces_the_expected_output_length \
	resample_block_boundaries_are_seamless \
	seek_after_resample_matches_a_fresh_decoder_seeked_to_the_same_position \
	end_to_end_resamples_a_44_1k_wav_through_pump_and_read_without_error \
	non_finite_samples_in_a_resampled_wav_do_not_panic_and_read_to_eof \
	decodes_a_non_48k_wav_by_resampling_to_the_output_rate

miri: miri-toolchain
	cargo +$(MIRI_TOOLCHAIN) miri test -p mw-core --target $(MIRI_TARGET) --lib -- $(MIRI_CI_FILTER) \
		$(addprefix --skip ,$(MIRI_CI_SKIP))

# 統合テスト(tests/realtime_safety.rs)は Miri では終わらない(カウンティングアロケータの下で
# 大量の描画を回すため 40 分を超えても1件目が終わらなかった)ので lib のテストだけにする。
miri-all: miri-toolchain
	cargo +$(MIRI_TOOLCHAIN) miri test -p mw-core --target $(MIRI_TARGET) --lib

miri-toolchain:
	@if ! rustup toolchain list | grep -q "^$(MIRI_TOOLCHAIN)"; then \
		echo "$(MIRI_TOOLCHAIN) が無いため導入します(miri / rust-src 付き)"; \
		rustup toolchain install $(MIRI_TOOLCHAIN) --profile minimal --component miri --component rust-src; \
	fi

# --- cargo-fuzz(ADR-0003 の2段目)-------------------------------------------
#
# 対象は wav::decode / decode.rs(WavDecoder の open とストリーミング供給)/ resample.rs の3本
# (`fuzz/fuzz_targets/`)。`fuzz/` はルートの workspace から切り離してある(`Cargo.toml` の
# `exclude`)ので、clippy / cargo-deny / test の対象には入らない。
#
# `cargo fuzz` は `fuzz/` を自動で見つけるため、どこから呼んでもよい(ここではルートから
# 呼ぶ)。nightly は Miri と同じ NIGHTLY_TOOLCHAIN を共有する(🔴 日付はそちらの変数だけに書く)。
FUZZ_TARGETS := wav_decode wav_decoder_pump resample

fuzz-toolchain: miri-toolchain
	@if ! cargo fuzz --version >/dev/null 2>&1; then \
		echo "cargo-fuzz が無いため導入します"; \
		cargo +$(NIGHTLY_TOOLCHAIN) install cargo-fuzz --locked; \
	fi

fuzz-build: fuzz-toolchain
	cargo +$(NIGHTLY_TOOLCHAIN) fuzz build

# コーパスの回帰実行だけ(-runs=0: 新規生成はせず、既存コーパスを1件ずつ流して
# クラッシュが無いことだけ見る。数秒で終わる)。**CI はこれを呼ぶ**(コーヒー基準)。
# 長時間のファジングは fuzz-run(手元)か週次ワークフロー(.github/workflows/fuzz-weekly.yml)で行う。
fuzz-corpus: fuzz-toolchain
	@for t in $(FUZZ_TARGETS); do \
		echo "== fuzz-corpus: $$t =="; \
		cargo +$(NIGHTLY_TOOLCHAIN) fuzz run $$t -- -runs=0 || exit 1; \
	done

# 1本を指定秒数だけ実行する(手元での探索・週次ワークフロー用。CI の push には載せない)。
# 例: make fuzz-run TARGET=wav_decode SECONDS=60
fuzz-run: fuzz-toolchain
	@if [ -z "$(TARGET)" ]; then \
		echo "使い方: make fuzz-run TARGET=<$(FUZZ_TARGETS)> [SECONDS=600]"; \
		exit 1; \
	fi
	cargo +$(NIGHTLY_TOOLCHAIN) fuzz run $(TARGET) -- -max_total_time=$(if $(SECONDS),$(SECONDS),600) -rss_limit_mb=2048

fuzz-seeds:
	python3 fuzz/seeds/make_seeds.py

# --- ThreadSanitizer(手元専用。ADR-0003 の3段目)-----------------------------
# 実デバイスの音声スレッドと制御スレッドの実行時の競合を見る(loom のモデル検証と補完関係)。
# nightly の -Zsanitizer=thread は std を作り直す(-Zbuild-std)ため初回は数分、flaky にもなりうるので
# CI には載せない。unsafe や スレッド間の共有構造(ring_buffer / clock / mixer の受け渡し)を触ったら1回回す。
#
# RUSTDOCFLAGS にも同じフラグを渡す —— cargo test はドキュメントテストを rustdoc 経由で
# 別途コンパイルするが、rustdoc は RUSTFLAGS を継承しない。渡さないと、既にサニタイザ付きで
# ビルド済みの依存 rlib(bitflags / coreaudio 等)に対して、サニタイザ無しでコンパイルされる
# ドキュメントテスト側の crate が ABI 不一致でエラーになる(実測で確認済み)。
TSAN_TARGET := $(shell rustc -vV | sed -n 's/^host: //p')
tsan: miri-toolchain
	RUSTFLAGS="-Zsanitizer=thread" RUSTDOCFLAGS="-Zsanitizer=thread" cargo +$(NIGHTLY_TOOLCHAIN) test -Zbuild-std --target $(TSAN_TARGET) --target-dir target/tsan --workspace

bench:
	@echo "criterion ベンチマークは未導入(初期構築仕様 §7.3: 任意)。M1 以降のミキサ実装後に追加する。"

# --- API リファレンス(M5)-------------------------------------------------
#
# 🔴 手書きの API 一覧は作らない(必ず実装から乖離する)。docs/integration.md §6 は
# この rustdoc 出力と csbindgen の NativeMethods.g.cs を「正」として指している。
# --no-deps: 依存クレートまで生成すると出力が巨大になり、見たいもの(mw-ffi の境界)が埋もれる。
# --document-private-items は付けない: 読者は C ABI の利用者で、内部実装は対象外。

doc: doc-coverage
	cargo doc -p mw-ffi -p mw-core -p mw-backend --no-deps
	@echo "generated: target/doc/mw_ffi/index.html"

# doc の前提(全エクスポート関数に doc コメントがある)を機械で見張る。
# 1つ欠けると API リファレンスに説明なしの関数が並ぶため、doc の依存にしてある。
doc-coverage:
	@python3 tools/check-ffi-doc-coverage.py

# csbindgen は mw-ffi の build.rs から実行される。ネイティブ(ホスト)向けビルドを
# 1回走らせるだけで unity/Runtime/Generated/NativeMethods.g.cs が再生成される
# (関数シグネチャはターゲットに依存しないため、ホストビルドで十分)。
bindgen:
	cargo build -p mw-ffi
	@test -f unity/Runtime/Generated/NativeMethods.g.cs \
		&& echo "generated: unity/Runtime/Generated/NativeMethods.g.cs" \
		|| (echo "NativeMethods.g.cs の生成に失敗しました" && exit 1)

# --- ネイティブビルド -----------------------------------------------------

# Intel Mac と Apple Silicon Mac のどちらの Unity Editor でも同じ配布物を使えるよう、
# macOS 用 dylib は両ターゲットを release ビルドし、lipo で universal に束ねる。
build-macos:
	cargo build -p mw-ffi --release --target aarch64-apple-darwin
	cargo build -p mw-ffi --release --target x86_64-apple-darwin
	@mkdir -p $(PLUGINS_MACOS_DIR)
	@# install name はビルドした機械の絶対パスになるので、配布物では @rpath に揃える
	@# (Unity はプラグインをパスで読み込むため、値そのものは読み込みに影響しない)。
	@for arch in aarch64 x86_64; do \
		install_name_tool -id @rpath/libmw_ffi.dylib target/$$arch-apple-darwin/release/libmw_ffi.dylib; \
	done
	@lipo -create \
		target/aarch64-apple-darwin/release/libmw_ffi.dylib \
		target/x86_64-apple-darwin/release/libmw_ffi.dylib \
		-output $(PLUGINS_MACOS_DIR)/libmw_ffi.dylib
	@lipo $(PLUGINS_MACOS_DIR)/libmw_ffi.dylib -verify_arch arm64 x86_64 \
		|| (echo "universal dylib に arm64 / x86_64 の両方が入っていません" && exit 1)
	@echo "built: $(PLUGINS_MACOS_DIR)/libmw_ffi.dylib($$(lipo -archs $(PLUGINS_MACOS_DIR)/libmw_ffi.dylib))"

build-ios:
	cargo build -p mw-ffi --release --target aarch64-apple-ios
	@mkdir -p $(PLUGINS_IOS_DIR)
	@rm -rf $(XCFRAMEWORK)
	xcodebuild -create-xcframework \
		-library target/aarch64-apple-ios/release/libmw_ffi.a \
		-output $(XCFRAMEWORK)
	@echo "built: $(XCFRAMEWORK)"

build-android:
	cargo ndk -t $(ANDROID_ABI) -P $(ANDROID_API_LEVEL) -o /tmp/mgct-mw-android-out \
		build -p mw-ffi --release
	@mkdir -p $(PLUGINS_ANDROID_DIR)
	@cp /tmp/mgct-mw-android-out/$(ANDROID_ABI)/libmw_ffi.so $(PLUGINS_ANDROID_DIR)/libmw_ffi.so
	@echo "built: $(PLUGINS_ANDROID_DIR)/libmw_ffi.so"
	@ls -la $(PLUGINS_ANDROID_DIR)/libmw_ffi.so

# --- パッケージング(M0 はスタブ。仕上げは M5) -----------------------------

package:
	@echo "UPM パッケージ組み立て(スタブ、初期構築仕様 M5 で本実装)。"
	@echo "現状は unity/ ディレクトリそのものが file: 参照可能な UPM パッケージとして機能する。"
	@missing=0; \
	for f in $(PLUGINS_MACOS_DIR)/libmw_ffi.dylib unity/Runtime/Generated/NativeMethods.g.cs; do \
		if [ ! -e "$$f" ]; then echo "missing: $$f"; missing=1; fi; \
	done; \
	if [ $$missing -eq 1 ]; then \
		echo "先に 'make build-macos' 'make bindgen' を実行してください。"; exit 1; \
	fi
	@echo "OK: 必須ファイルは揃っています(unity/package.json, Runtime/Generated, Runtime/Plugins/macOS)。"

# --- Unity 統合検証 --------------------------------------------------------
#
# 重要: いかなる Unity 起動もマシン全体で直列化すること。
# tools/with-unity-lock.sh が /tmp/mgct-unity.lock を until ループで取得し、
# 実行後に必ず解放する(並行して動く他セッションもクライアントプロジェクト側で
# Unity を使うため)。

unity-sample-create:
	@if [ -d "$(UNITY_SAMPLE_DIR)/Assets" ]; then \
		echo "$(UNITY_SAMPLE_DIR) は既に存在します(スキップ)。作り直す場合は削除してから再実行してください。"; \
	else \
		$(UNITY_LOCK_RUNNER) "$(UNITY_APP)" -batchmode -nographics \
			-createProject "$(CURDIR)/$(UNITY_SAMPLE_DIR)" \
			-quit -logFile -; \
		echo "作成後、Packages/manifest.json に以下を追記すること(初回のみ、手動 or ツールスクリプト):"; \
		echo '  "com.mu5dvlp.open-audioware": "file:../../unity"'; \
		echo '  "com.unity.test-framework": "1.6.0"'; \
	fi

unity-test:
	$(UNITY_LOCK_RUNNER) "$(UNITY_APP)" -batchmode -nographics \
		-projectPath "$(CURDIR)/$(UNITY_SAMPLE_DIR)" \
		-runTests -testPlatform EditMode \
		-testResults "$(CURDIR)/$(UNITY_SAMPLE_DIR)/EditModeTestResults.xml" \
		-logFile -
	@echo "results: $(UNITY_SAMPLE_DIR)/EditModeTestResults.xml"

# --- M1 A/B 計測(docs/measurement-m1.md)------------------------------------
# 手順書 §4.1 が -executeMethod の手打ちを求めていたのをターゲット化したもの。
# どちらも Unity を起動するため必ずロック経由で実行する。

measurement-scene:
	$(UNITY_LOCK_RUNNER) "$(UNITY_APP)" -batchmode -nographics \
		-projectPath "$(CURDIR)/$(UNITY_SAMPLE_DIR)" \
		-executeMethod Measurement.EditorTools.MeasurementSceneBuilder.Build \
		-quit -logFile -

# 事前に make build-ios(xcframework)と make bindgen を済ませておくこと。
measurement-export-ios:
	$(UNITY_LOCK_RUNNER) "$(UNITY_APP)" -batchmode -nographics \
		-projectPath "$(CURDIR)/$(UNITY_SAMPLE_DIR)" \
		-executeMethod Measurement.EditorTools.IosXcodeExporter.Build \
		-quit -logFile -
	@echo "書き出し先: $(UNITY_SAMPLE_DIR)/Build/iOS/Unity-iPhone.xcodeproj"

# 事前に make build-android(.so)と make bindgen を済ませておくこと。
measurement-build-android:
	$(UNITY_LOCK_RUNNER) "$(UNITY_APP)" -batchmode -nographics \
		-projectPath "$(CURDIR)/$(UNITY_SAMPLE_DIR)" \
		-buildTarget Android \
		-executeMethod Measurement.EditorTools.AndroidApkExporter.Build \
		-quit -logFile -
	@echo "書き出し先: $(UNITY_SAMPLE_DIR)/Build/Android/measurement.apk"

# --- 掃除 ------------------------------------------------------------------

clean:
	cargo clean
