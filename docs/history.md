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
| M3 実運用耐性 | **着手**(iOS の割り込み・バックグラウンド復帰・ルート変化〔OldDeviceUnavailable〕は実装済み。Android の AAudio disconnect 再オープン・アンダーラン検知テレメトリは未着手) |
| M4 仕上げ機能 | **着手中**(M4-1 予約発音の C# ラッパ 完了 / M4-2 キャリブレーション連携 完了) |
| M5 ハードニング | 未着手 |

⚠️ **M3 を飛ばして M4 を進めている。** テンプレート側から `AudioSource` を剥がすのに
必要な機能(キャリブレーション・BGM・プレビュー)が M4 に集まっているため、
ユーザー方針「AudioSource には頼らず全面 middleware へ移行したい」に沿って順序を入れ替えた。

## 作業記録

#### 2026-08-28(middleware: M3 追撃 —— iOS 実機バグ修正その2「Bluetooth 解除で SE が無音になる」)

**症状**(ユーザー実機報告その2): Bluetooth で接続してから Bluetooth を解除すると SE が
鳴らなくなる。前回の割り込み対応(`ios_interruption.rs`)では塞げていなかった。

**なぜ前回の修正で直らなかったか**: 前回監視したのは
`AVAudioSessionInterruptionNotification`(電話着信等)と
`UIApplicationDidBecomeActiveNotification`(安全網)の2つ。**ルート変化は別の通知
(`AVAudioSessionRouteChangeNotification`)で飛んでくる**ため対象外だった。

**cpal がルート変化時に何をしているか(ソースを読んで確認)**:
`coreaudio::ios::session_event_manager.rs::route_change_error` は reason を分類して
`host/error_emit.rs::emit_error`/`try_emit_error` 経由でユーザーの `error_callback`
(＝このクレートが渡した `err_fn`)を呼ぶだけ。`emit_error`/`try_emit_error` の実装は
コールバックを呼ぶ1行のみで、**`AudioUnit` にも `Stream::play()` が見る
`playing` フラグにも一切触れない**。つまり cpal はルート変化時にストリームの作り直しも
`stop`/`start` の呼び直しも一切せず、二重処理の心配は無い——復帰は完全にこちら側の責務、
かつ前回発見した「`playing` フラグが OS 主導の停止に追随しない」問題がルート変化にも
そのまま当てはまる(cpal が一切関与しないので当然)。

**復帰を走らせる reason は `AVAudioSessionRouteChangeReason::OldDeviceUnavailable` のみ**
(BT 切断・イヤホン抜け相当。Apple のドキュメント上「直前まで使っていたデバイスが
無くなった」で実機報告と一致し、cpal 自身もこの reason だけを他と別グループに分類して
いる)。`NewDeviceAvailable`(BT 接続等)・`Override` は音が途切れず自動継続するのが通例
のため復帰を試みない(依頼書の警告どおり、正常なルート切替のたびに音切れを生むのを
避ける)。`CategoryChange` は `ios_session::configure()` 自身が引き起こしうるため
自己誘発ループを避けて反応しない。`NoSuitableRouteForCategory` 等は復帰しても
改善しないため見送り。判断根拠は `ios_interruption.rs` のモジュール doc
「追記: ルート変化」に詳述。

**実装**: 同じ `ios_interruption.rs` に3つ目の observer
(`AVAudioSessionRouteChangeNotification`)を追加。`InterruptionState` に
`on_route_changed(RouteChangeReason)` を追加し(復帰要求 reason ならどの状態からでも
`RecoveryPending` へ、それ以外は状態を一切変えない)、判定ロジック自体は
`RouteChangeReason::requires_recovery`(OS 呼び出しを含まない純粋な値型、imp 側が実際の
`AVAudioSessionRouteChangeReason` から変換するだけ)に切り出した。復帰処理
(`pause()`→`play()`)は既存の `attempt_recovery` をそのまま再利用。

**ついでに初期構築仕様『§6』の宿題を1つ片付けた**: `mw_core::Event::RouteChanged` は
M1 の時点で「型だけ用意し発火は繋がない」と明記されていた予約済みイベント
(テンプレート側のオフセット自動再較正用)。今回ルート変化の検知経路ができたので、
reason を問わずルート変化のたびに(`requires_recovery` の判定とは独立に)これも
発火させるようにした。**既存の `MwEventKind::RouteChanged = 0` の値は変えていない**
(元々予約されていた判別子をそのまま使うだけなので、新しい列挙子の追加すら不要だった)。

**テスト**: `ios_interruption.rs` に4件追加(`OldDeviceUnavailable` はどの状態からでも
`RecoveryPending` へ / それ以外の reason(`NewDeviceAvailable`/`CategoryChange`/
`Override`/`Other`)はどの状態も変えない/`requires_recovery` は `OldDeviceUnavailable`
だけ true / ルート変化経由の復帰失敗もアクティブ化での再試行に乗る)。
`realtime_safety.rs` は今回も無変更。

自動テストで守れるのは reason ごとの状態遷移ロジックのみ。**実機で BT 接続→切断の
シーケンスを試し、SE が復帰することの確認は依頼書のとおり実機検証でしか確認できない。**

検証: `cargo fmt --check` 緑 / `cargo clippy --workspace --all-targets -- -D warnings`
警告0(ホスト・`aarch64-apple-ios` の両ターゲット) / `cargo test --workspace`
**219/219 緑**(前回基準 215 から +4 = 今回追加した単体テスト4件ぶん。内訳
mw-backend 13 / mw-core 153 / mw-core realtime_safety 1 / mw-ffi 52) /
`cargo build --workspace --release`(ホスト)・`cargo build -p mw-ffi --release
--target aarch64-apple-ios` いずれも成功。C ABI の関数シグネチャ・列挙子の値は
無変更(doc コメント更新のみ)。

#### 2026-08-28(middleware: M3 着手 —— iOS 実機バグ修正「バックグラウンドから戻ると SE だけ無音になる」)

**症状**(ユーザー実機報告。iPhone 14 / iOS): タスクをキルせずホームへ戻り、ゲームへ戻ると
SE だけ無音になる。ボイス(Unity `AudioSource` 経由)は鳴る。

**原因**: `crates/mw-backend/` にオーディオ割り込み・アプリライフサイクルの処理が一切無かった。
cpal 0.18.1 の iOS 実装(`coreaudio::ios::session_event_manager`。ソースを確認済み)は
ルート変化とメディアサービスの喪失/リセットしか監視しておらず、
**`AVAudioSessionInterruptionNotification` を一切監視していない**。iOS はアプリが
(Background Audio 機能を持たないまま)バックグラウンドへ行くと `AVAudioSession` を
非アクティブ化して RemoteIO の出力ユニットを止めるが、この通知が来ないため
ミドルウェア側は何も気づかず、`cpal::Stream` は「鳴っているつもり」のまま固まる。SE は
このストリーム経由なので鳴らなくなり、Unity 自身が管理するボイスだけ生き残る——
「SE だけ鳴らない」という報告と正確に一致する。

さらに、cpal 0.18.1 の `Stream::play()`(iOS 実装)は内部の `playing` フラグが既に `true`
なら `AudioOutputUnitStart` を呼ばずに即 return する。OS が横から止めても cpal 側はそれを
検知しないため**このフラグは追随せず、単純に `play()` を呼び直すだけでは復帰しない**
(先に `pause()` でフラグを倒す必要がある。詳細は `ios_interruption.rs` のモジュール doc)。

**実装方針**(検討した3案のうち3を採用。理由は `ios_interruption.rs` 冒頭に詳述):

1. cpal の `Stream`/`Backend` を閉じて再オープン —— `Renderer` を値渡し(ムーブ)で
   音声コールバックへ排他所有させる現行設計(M1)では、一度ムーブした `Renderer`
   (=全ボイス・バス音量・楽曲再生位置)を取り戻す経路が無く、再オープンすると
   まるごと失われる。これを直すには `mw-ffi::handle::Instance` 側の大きな再設計が要る
   (初期構築仕様§14 の「AAudio 再オープンは M3 で必須実装」の本丸そのもの)。今回の
   スコープ(実機報告の再現・修正)を超えるため見送った
2. 最小の Obj-C シムを xcframework に同梱 —— cpal 0.18 は既に `objc2` 系クレートを
   iOS 実装の依存に持ち込んでおり(`ios_session.rs` が既に利用中)、Obj-C ファイルを
   増やすと「Obj-C シムは薄く保つ」方針にも反する
3. **(採用)同じ `cpal::Stream` を維持したまま、`objc2-foundation::NSNotificationCenter`
   へ Rust から直接 observer を登録し、復帰時に `pause()`→`play()` を呼び直す。**
   cpal 自身の `session_event_manager.rs` が全く同じパターン(ブロックベースの
   observer 登録)を使っており実績がある。`Renderer` の所有権を一切動かさないため、
   ボイス・バス音量・楽曲位置は割り込みを跨いで自然に保持される(最小の変更で
   実機報告のバグそのものを直せる)

監視する通知は2つ: `AVAudioSessionInterruptionNotification`(電話着信等の「真の」割り込み。
Ended の `userInfo` の `AVAudioSessionInterruptionOptionShouldResume` を見て復帰要否を判断)と、
`UIApplicationDidBecomeActiveNotification`(安全網。バックグラウンド遷移では Began は確実に
飛ぶが、対応する Ended が確実に飛んでくる保証が無いため)。後者は**割り込み中だった場合に
限り**復帰を試みる——Control Center やバナー通知でも `DidBecomeActive` は飛んでくるが実際には
中断していないため、無条件に `pause()`→`play()` すると通常プレイ中に不要な音切れを生む。
通知名は `objc2-ui-kit` を新規依存に追加せず `ns_string!` マクロで文字列リテラルから直接
作った(実行時アロケーション無し)。

**Android の点検結果**: cpal 0.18 の AAudio 実装は disconnect を `err_fn` 経由で
`Event::StreamError` として既に報告できる(M2-6 の既存経路)。ただし AAudio の disconnect は
iOS と違い**構造的に終端**(disconnect したストリームは `play()` で復活せず、閉じて
新しいストリームを作るしかない)で、これは案1で見送った「`Renderer` の再構築」問題と
完全に同じ壁にぶつかる。今回のスコープでは見送り、報告のみとした(依頼書の指示どおり)。

**C ABI へのイベント追加**: `mw_core::Event` に `AudioInterruptionBegan` /
`AudioInterruptionEnded { recovered: bool }` を追加し、`mw_poll_events` 経由で観測できるように
した(`MwEventKind` へ 6/7 を追加。**既存シグネチャは無変更**、列挙値の追加のみ)。
クライアント側が「鳴らなくなった/復帰した(成功したか)」を知る手段がこれまで無かったための
追加(依頼書の判断基準どおり)。

**テスト**: 実機の割り込み・バックグラウンド遷移は自動テストで再現できないため、復帰ロジックを
`InterruptionState`(OS API 呼び出しを一切含まない純粋な状態機械。`Running` →
`Interrupted` → `RecoveryPending` → `Recovered`/`RecoveryFailed`)として切り出し、
`crates/mw-backend/src/ios_interruption.rs` に7件の単体テストで遷移を固定化した
(「停止した→再開要求→再開した」の基本シナリオ、`ShouldResume` 無しでは復帰を試みない、
復帰失敗後の再試行、ベニンなアクティブ化では何もしない、新しい割り込みは常に最優先、
既定値)。`realtime_safety.rs` は変更していない(音声スレッド経路は今回無変更)。

自動テストで守れる範囲は状態機械の遷移ロジックのみ。**OS 通知が実機で実際に発火するか**
**`pause()`→`play()` の順序で `AudioOutputUnitStart` が本当に音を復活させるか**は
実機検証でしか確認できない(iPhone 14 実機でホーム往復を試すこと)。

検証: `cargo fmt --check` 緑 / `cargo clippy --workspace --all-targets -- -D warnings` 警告0
(ホスト・`aarch64-apple-ios` の両ターゲットで確認) / `cargo test --workspace`
**215/215 緑**(内訳 mw-backend 9 / mw-core 153 / mw-core realtime_safety 1 / mw-ffi 52。
着手前基準 208 から +7 = `ios_interruption.rs` の新規ユニットテスト7件ぶん) /
`cargo build -p mw-ffi --release --target aarch64-apple-ios` 成功(実際の iOS ターゲットで
objc2 コードがビルドできることを確認。xcframework の組み立て・Unity への配置・実機インストールは
呼び出し元が行う)。

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
