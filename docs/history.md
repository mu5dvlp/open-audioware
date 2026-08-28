# 実装の経緯

マイルストーン(M0〜M5)の定義と終了条件は初期構築仕様(`init.md`)の §10 が正。
計測結果は [`measurement-m1.md`](measurement-m1.md) が正。
本ドキュメントは<b>いつ・なぜそうしたか</b>の記録。

> 元は作業ディレクトリ直下の `HANDOFF.md` に溜めていたが、再開時に読む文書として
> 肥大化したため 2026-08-26 にここへ移した。

## 到達状況(2026-08-26)

| Milestone | 状態 |
|---|---|
| M0 基盤 / M1 SE 再生 / M2 楽曲とクロック | **完了** |
| M3 実運用耐性 | **未着手**(⚠️ 実機運用に入る前には必須。ルート変化・割り込み・アンダーラン検知) |
| M4 仕上げ機能 | **着手中**(M4-1 予約発音の C# ラッパ 完了 / M4-2 キャリブレーション連携 完了) |
| M5 ハードニング | 未着手 |

⚠️ **M3 を飛ばして M4 を進めている。** テンプレート側から `AudioSource` を剥がすのに
必要な機能(キャリブレーション・BGM・プレビュー)が M4 に集まっているため、
ユーザー方針「AudioSource には頼らず全面 middleware へ移行したい」に沿って順序を入れ替えた。

## 作業記録

#### 2026-08-28(middleware: バグ修正 —— キャリブレーション画面キャンセルでメトロノームが鳴り続ける退行)

**症状**(ユーザー実機報告): キャリブレーション画面でキャンセルを押してもメトロノームが
鳴り続ける。

**原因**: 予約された SE は `Mixer.voices`(いま鳴っているボイス)と
`Mixer.se_schedule`(`ScheduleQueue<ScheduledSe>`、これから鳴る未発火の予約)の
2箇所に分かれて存在するが、`Command::StopVoice`/`Command::StopVoicesUsingSound` の
処理が `voices` 側しか止めておらず、`se_schedule` に積まれた未発火の予約が
素通りしていた。既定実装(Unity `AudioSource`)では `Destroy` で予約ごと消えるため、
ミドルウェア導入で初めて顕在化した退行。呼び出し側(Unity/C#)のロジックは正しかった。

**修正**:

- `crates/mw-core/src/schedule.rs`: `ScheduleQueue<T>` に `remove_where(matches, on_removed)`
  を追加。取り除いた要素は `on_removed` へそのまま渡す(その場で drop しない)契約にし、
  `entries.remove(i)` の詰め直しだけで昇順不変条件を保ったまま任意条件の削除ができるようにした
  (`Vec::remove` は再アロケーションを起こさないため §5.3 に抵触しない)。
- `crates/mw-core/src/mixer.rs`: `Command::StopVoice`/`Command::StopVoicesUsingSound` の
  処理で、`voices.stop()`/`voices.stop_all_using_sound()` に加えて
  `se_schedule.remove_where(...)` も呼ぶようにした。取り除いた `ScheduledSe` が保持する
  `Arc<SoundData>` は既存の回収キュー(`ReclaimSender::send_or_leak`)へそのまま渡し、
  音声コールバック内での Arc ドロップを発生させない(`voice.rs` の Arc 所有権設計を
  そのまま踏襲。実際のデアロケーションはゲームスレッド側の `ReclaimReceiver::drain` で
  起こる)。
- `crates/mw-core/tests/realtime_safety.rs`: 既存のカウンティングアロケータ統合テストへ
  「予約 → 直後に StopVoice/StopVoicesUsingSound でキャンセル」のシナリオを追加し、
  この削除経路も0アロケーション/0デアロケーションであることを実測で固定化した。

**テスト**(`crates/mw-core`、依頼書の5要件に対応): `schedule.rs` に `remove_where` 自体の
単体テスト4件(一致要素のみ削除して `on_removed` へ渡す/昇順の生存者が壊れない/
不一致時は無変更/全削除で空になる)、`mixer.rs` に発火前キャンセルで鳴らない・
別 voice は巻き添えにならない・発火済み voice への `StopVoice` は従来どおり効く(退行なし)・
`StopVoicesUsingSound` で同一音源の未発火予約も消える・削除後も `fire_due_se` が
正しいオフセットで発火する(昇順不変条件の保持)・キャンセル分の `Arc` が回収キュー経由で
渡ること、の計6件を追加。

検証: `cargo fmt --check` 緑 / `cargo clippy --workspace --all-targets -- -D warnings` 警告0 /
`cargo test --workspace` **208/208 緑**(内訳 mw-backend 2 / mw-core 153 / mw-core
realtime_safety 1 / mw-ffi 52。着手前基準 198 から +10 = 新規ユニットテスト10件ぶん)。
公開 API(C ABI)のシグネチャは無変更のため `make bindgen` は不要。

#### 2026-08-24〜25(middleware: M2-7 完了 —— 楽曲制御 API を FFI へ公開)

3コミットに分けた。下から上へ積む形で、各コミット単体でもビルドが通る。

| コミット | 層 | 内容 |
|---|---|---|
| `31e96b8` | mw-backend | 出力レイテンシの実測値を公開(`mw_get_output_latency_ns` の供給元) |
| `f33996f` | mw-core | 楽曲制御コマンド4種を追加 / 音楽クロックに4状態を載せる |
| `b45fbf8` | mw-ffi | デコードスレッド + FFI 9関数 + csbindgen 再生成 |

検証: `make lint` 緑 / `cargo test --workspace` **198/198 緑**(実機の音声デバイスを使う
end-to-end の楽曲ライフサイクルテストを含む)/ `make bindgen` 成功。

**設計上、後から効いてくる決定**:

- **4状態(Loading/Ready/Playing/Paused)は seqlock の内側に入れた**。外に独立した
  atomic を置くと `is_playing` と状態が食い違ったスナップショットが読めてしまい、
  seqlock を導入した目的(複数フィールドの整合)そのものが崩れる
- **`publish` の引数から `is_playing` を外し、状態から導出する形にした**。呼び出し側が
  両方を別々に渡せると、そもそも食い違った値を publish できてしまうため
- **デコードスレッドは「1本立てて、デコーダを差し替える」方式**。曲ごとに立て直さない。
  待ちは 10ms のポーリング(条件変数にしないのは、起こす側の半分が音声スレッドであり
  そこから notify できないため)
- **楽曲 ID は最上位ビットで SE と空間分離**。楽曲は「圧縮のまま保持」で SE の
  デコード済み PCM とは置き場所が違い、ID を共有すると `mw_sound_release` が
  SE 側を巻き添えに消しうる
- **`MwMusicPosition` の bool 相当は `u8`**。C# の `bool` は既定で4バイトに
  マーシャリングされ、Rust の1バイト `bool` とレイアウトが合わない。毎フレーム呼ぶ
  API で GC アロケーションを出さないための措置
- **csbindgen の落とし穴**: enum は「extern 関数のシグネチャから(構造体フィールド経由でも)
  到達可能」でないと C# 側に生成されない。`build.rs` の入力に足すだけでは出ない

**⚠️ この作業でオーケストレータ(私)の指示が間違っていた**。詳細は下の「学び」節に記載。


#### 2026-08-23(middleware: M1 完了 → M2 着手)

**M1 は iOS / Android とも成功基準を達成**(docs/measurement-m1.md §8.5 / §9.8)。
Android の A/B は差 **126.7ms**・ジッタ 10.1 → 2.0ms。絶対値の見積もりから逆算した表示遅延が
iOS(iPad)で独立に逆算した値とほぼ一致(43.6ms vs 43.9ms)し、モデルの傍証になった。
**M6(Android を oboe 直叩きへ置換)の前倒しは不要**と判断。

**M2 の実装順**(エージェント側の分解):

| | 内容 | 状態 |
|---|---|---|
| M2-1 | 音楽クロックのスナップショット(seqlock + 世代カウンタ) | **完了** `f8adc7c` |
| M2-2 | 楽曲ボイスの再生状態機械(ポーズ / 巻き戻し再開 / シーク / ループ区間) | **完了** `7d98c61` |
| M2-3 | Symphonia ストリーミングデコード + リングバッファ | **完了** `bfd4ac7` |
| M2-4 | リサンプル(rubato) | **完了** `3ba4bfc` |
| M2-5 | 予約再生 + デバイスタイムスタンプ相関 | **完了** `7ce2897` |
| M2-6 | イベントキューと `mw_poll_events` | **完了** `4dd2f61` |
| M2-7 | FFI 公開 + csbindgen 再生成 | **完了**(2026-08-24〜25。`31e96b8` `f33996f` `b45fbf8` の3コミット) |
| M2-8 | **クライアント側 `App.Audio.Native` アダプタ統合**(M2 後半) | **実装完了**(2026-08-25。middleware `70ff77d` + client #98)。**残りは実機計測のみ** |

設計上の決定:

- **デコードスレッドは mw-core に持たせない**。mw-core は「オフラインレンダリングだけで完結し
  テストの主戦場になる」層なので、`MusicDecoder`(同期) + SPSC リングバッファ +
  `pump()` に分け、**スレッドは外側(mw-ffi)が回す**。テストはスレッド無しで全経路を通せる
- **PCM の供給元は `MusicFrameSource` トレイトで抽象化**。M2-3 の Symphonia 実装が差し込まれても
  M2-2 の状態機械は変更不要
- Symphonia も MPL-2.0 のため、M1 で行った MPL-2.0 許可の判断が M2-3 でも効く

**⚠️ M2-8 でやり直すべき比較(再開時に必ず読むこと)**: iOS 計測(§8.4)で、
ミドルウェアが要求した I/O バッファ長は **Unity の出力経路にも効く**ことが判明している
(A も 189→106ms と縮んだ)。client 側の `ProjectSettings/AudioManager.asset` の
`m_DSPBufferSize` は **#95(2026-08-23)で 256(Best Latency 相当)に変更済み**だが、
**その状態での A/B 計測はまだ行っていない**。

つまり **記録されている差 iOS 137.8ms / Android 126.7ms は「チューニング前の Unity」との
比較であり、フェアな数字ではない**。M2-8 でクライアント統合したら、**チューニング後の
Unity と比べ直すこと**。ミドルウェアの優位は現在の記録より縮む可能性が高い。
なお 256 に詰めたことによるアンダーラン耐性の劣化も未計測。


#### 2026-08-23(middleware: Android M1 計測に着手 —— 不具合3件を修正)

**Android では診断ログがどこにも出ていなかった**ため、「ミドルウェアが Android で一度も
音を出していなかった」ことに誰も気づいていなかった。ログ経路を作った途端に不具合が連鎖的に出た。

1. **`platform_log`** — Android は user ビルドでプロセスの stderr が破棄される
   (`log.redirect-stdio` は SELinux で設定不可)。`eprintln!` を OS ごとに振り分ける形にし、
   Android は `liblog` の `__android_log_write` へ流す(`adb logcat -s mw:V` で読める)
2. **サンプルレート選択** — `find_f32_stereo_config` が f32 ステレオ構成の**列挙の先頭**の
   最大レートを採っていた。cpal 0.18 の Android 実装は 5512Hz から列挙するため
   `InvalidRate` で `mw_init` が失敗していた。デバイス既定構成を最優先する形に変更
3. **`ndk_context` 未初期化** — cpal の AAudio 実装は全経路で Java 側の `AudioManager` を
   参照するため JavaVM + Android Context を要求するが、Unity のようなホストアプリでは
   誰も初期化しない → `mw_init` が `ErrPanic`。`JNI_OnLoad` で JavaVM を控え、
   `ActivityThread.currentActivityThread().getApplication()` で Context を取って登録する
   `android_context` を新設(**ホスト側に一切の協力を求めない**= .so を置くだけで動く)。
   併せて panic の内容も logcat へ流すフックを追加(`catch_unwind` が中身を消していた)

**計測結果**(SH-M16 / Android 11): コールバックバッファが
**886 frames = 18.46ms → 96 frames = 2.00ms(9.2倍)**。差は cpal の `realtime` フィーチャ
(= `AudioPerformanceMode::LowLatency`)の有無だけ。`dumpsys media.audio_flinger` でも
FastMixer 経路にトラックが乗ったことを確認。§8.7 と同じモデルで **2.0〜4.0ms + HW 出力
レイテンシ**、目標 ≤40ms には収まる見込み(HW 出力レイテンシは未測定)。

**ライセンス判断【確定】**: `realtime` フィーチャが `audio_thread_priority`(**MPL-2.0**)を
引き込み `deny.toml` で弾かれたため、**ユーザー判断で MPL-2.0 を許可リストへ追加**。
MPL-2.0 はファイル単位のコピーレフトで、リンクして使う限り利用側コードには伝播せず
ソース公開義務は生じない。見送った代替は「`BufferSize::Fixed` で代替」(performance mode が
効かないため改善が得られない)と「M6: oboe 直叩きへ前倒し」(作業量が見合わない)。

さらに**計測アプリ側にも1件**(`4afe4a0`。§7.6-5): 計測シーンに**カメラが1台も無く**、
フレームバッファがクリアされないため**白フラッシュがバッファに焼き付いて画面が永久に明滅**
していた。ボタンは毎フレーム描き直されるので正常に見え、白いステータス文字だけが白地に
溶けて消えるため「上の帯だけ明滅」に見える紛らわしい症状だった。クリア専用カメラを1台
置いて解決(実機で 2タップ → 発光2回、輝度差 176.4 を確認)。
**⚠️ 計測の基準信号そのものが壊れていたため、A/B 動画は必ずこの修正後の apk で撮ること。**
iOS の §7/§8 の結果への影響は要確認(§7.6-3 の「輝度差 34.4」はこの焼き付きと整合する)。

検証: `make lint`(ホスト / aarch64-linux-android 両ターゲット)・`make test` 6スイート全緑・
`make unity-test` 9/9 緑。記録は `docs/measurement-m1.md` の §6(TODO を全面再編)・
§7.6-5(明滅バグ)・§9(Android 実測)。
