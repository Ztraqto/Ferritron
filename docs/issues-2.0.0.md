# Ferritron 2.0.0 の問題点の洗い出し

2026-11-10、ブランチ `ferritron-2.0`（`c72816c`）を対象にした調査の記録です。修正はしていません。
続き（WebAssembly・LV2・スタンドアロン・ストリーム・声・設定・MCP・Web エディタ）は [issues-2.0.0-part2.md](issues-2.0.0-part2.md)、
その 3（ホットリロード・GPU エディタ・GPU 処理・FNG）は [issues-2.0.0-part3.md](issues-2.0.0-part3.md)、
機能の要望は [feature-requests.md](feature-requests.md) にあります。

## 調べた範囲と方法

- **全文を読んだもの**: `ferritron-core` の `abi.rs`（`process` の経路、状態の保存と読み込み）、`plugin.rs`、`params.rs`、
  `smooth.rs`、`bypass.rs`、`transport.rs`、`instance.rs`、`notify.rs`、`worker.rs`、`state.rs`、`audio.rs`、`presets.rs`、
  `parallel.rs`（前半）、VST3 シム（全体）、CLAP シム（`process`・状態・パラメータ・ファクトリ）、VST2 シム（イベントと `process`）、
  隔離のクライアント（`process`・状態）、マクロの `ParamSet` 生成、マニフェストの ID 検査。
- **実行して確かめたもの**: 一時的な結合テスト（`Harness` 経由、実行後に削除済み）で 5 件を再現。下の表の「再現」列が「実行」のもの。
  debug と release の両方で実行。
- **既存のテスト**: `cargo test -p ferritron-core -p ferritron-dsp -p ferritron-manifest` はすべて合格（失敗 0）。
- **読んでいない、または一部しか見ていないもの**: AU / AUv3 / AAX / LV2 / スタンドアロン / WAM の各アダプタ、`ferritron-web`、
  `ferritron-gpu-view`、`ferritron-gpu` / `-graph` / `-slang` / `-vulkan`、`ferritron-wasm`、`ferritron-native-graphics`、
  `ferritron-ara`、`ferritron-cli`、プロジェクトロック（非公開）、ホットリロード、`ferritron-dsp` の大半（`delay.rs`・`oversampler.rs` のみ読んだ）。
  **これらに問題がないという意味ではありません。**
- 「再現」が「読解」のものは、コードを読んで確かめただけで、実行していません。DAW では何も実行していません。

## 一覧

| # | 重要度 | 内容 | 再現 |
|---|---|---|---|
| A1 | 高 | `load_values`（プリセット）の値が、次の `process` まで反映されない。その間の `get_param`・状態の保存・ユーザープリセットの保存は古い値 | 実行 | （修正済み: T5）
| A2 | 高 | スリープ中のインスタンスがサイドチェーンの音で起きない | 実行 | （修正済み: T3）
| A3 | 高 | ブロックを分割したとき、`ctx.transport()` の位置が区間ごとに進まない | 実行 | （修正済み: T4）
| A4 | 高 | `"sample"` モードで、ブロック末尾（offset ≥ frames）のパラメータ変更の後のノートが消える。debug ビルドでは panic して以後ずっと無音 | 実行 | （修正済み: T1）
| A5 | 高 | CLAP: `reset` を無視、`NOTE_END` を送らない、`NOTE_CHOKE` を無視。ABI に reset がない | 読解 |
| A6 | 高 | シムのイベント上限を超えたとき、パラメータの最終値が狂い、ノートが捨てられる | 読解 | （修正済み: T40（VST3・CLAP））
| B1 | 中 | プラグインのコードの panic を受け止めていない経路がある（ホストごと落ちる） | 読解 |
| B2 | 中 | バイパス: panic でクロスフェード用バッファが返らない。完全バイパス中はノートオフが届かず、残響が凍る | ノートオフは実行、他は読解 | （一部修正: T8、プールのバッファが返らない件）
| B3 | 中 | `ferritron-notify`・`ferritron-watch` スレッドが終わらず、Windows では DLL を固定していない | 読解 |
| B4 | 中 | 通知のフォールバックが、メインスレッドが 100 ms 止まると別スレッドでハンドラを呼ぶ | 読解 | （修正済み: T23）
| B5 | 中 | 平滑化したパラメータで `ctx.changed()` が最初の 1 回しか真にならない | 実行 | （修正済み: T2）
| B6 | 中 | VST3 の 30 ms タイマーがスレッドタイマー。メインスレッド以外で作られると動かない。Linux にはポーリングがない | 読解 |
| B7 | 中 | 状態の保存で `save_state` を 2 回呼ぶ間にサイズが変わると、ゼロ埋めの状態が保存される（VST3・CLAP・VST2・AU） | 読解 | （修正済み: T41）
| B8 | 中 | `ctx.midi_out`・`ctx.set_param` の出力は VST3 と CLAP だけ。他の形式と隔離では黙って捨てる | 読解 |
| B9 | 中 | `[vst3] controller_id` のコントローラが完全な 2 個目のインスタンスになる | 読解 |
| B10 | 中 | 隔離: 1 ブロックが 250 ms を超えると子プロセスを止める（オフラインでも同じ） | 読解 |
| B11 | 中 | 状態の読み込みは、状態にないパラメータを既定値に戻さない | 実行 | （修正済み: T6）
| P1〜P11 | 性能 | 下の「性能」の章 | — |
| C1〜C10 | 低 | 下の「小さな問題」の章 | — |

---

## A. 重要度が高いもの

### A1. `load_values` の値が次の `process` まで反映されない（修正済み: T5）

- 場所: [instance.rs:668](../crates/ferritron-core/src/instance.rs)（`load_values`）、[instance.rs:370](../crates/ferritron-core/src/instance.rs)（`apply_bulk`）、
  [abi.rs:1545](../crates/ferritron-core/src/abi.rs)（呼び出しは `process_with` だけ）。
- 内容: `load_values` は値を `bulk` に置くだけで、パラメータのセルには書かない。セルに入るのはオーディオスレッドの次のブロックの先頭。
  一方で `load_values` はその場で `REQUEST_VALUES` を立て、ホストに「値が変わった」と伝える。
- 実行結果: `load_values(amount = 3)` の直後、`process` の前に読むと `param = 2`。その時点で保存した状態を別のインスタンスに読ませると `amount = 2`。
  `process` を 1 回呼ぶと 3 になる。
- 影響:
  - ホストが `process` を呼んでいない間（停止中に処理を止めるホスト、非アクティブ、スリープを尊重する CLAP ホスト）にプリセットを選んで保存すると、
    プリセットが保存されない。
  - `kParamValuesChanged` / `rescan(VALUES)` を受けたホストが、オーディオスレッドより先に値を読み直すと古い値を表示する
    （ブロックが長いとき、オフライン時に起きやすい）。
  - `presets::save_user` を `load_user` の直後に呼ぶと古い値を保存する。
- 直し方の案: `get_param`・`save_state`・`InstanceHandle::value` が保留中の `bulk` を先に見る。または、処理していないと分かるときはメインスレッドで適用する。

**現在の状態**: 第 1 回の T5 で直した。`get_param`・`save_state`・`InstanceHandle::value` は、次のブロックを待たずに保留中の `bulk` の値を読む（`instance.rs` の `pending_value`、`abi.rs` の `get_param` と `save_state`）。確かめたこと: `audit_fixes.rs` の `a1_loaded_values_are_visible_before_the_next_block`（`examples/multiband-test` のテストの期待は、この仕様に合わせて 1 件変えた）。残っていること: `process` が呼ばれない間のプリセットの保存は、ホストで確かめていない。

### A2. サイドチェーンがスリープを起こさない（修正済み: T3）

- 場所: [abi.rs:1619](../crates/ferritron-core/src/abi.rs) `let input_quiet = is_quiet(channels, ...)`。主バスだけを見ている。
- 実行結果: サイドチェーンを出力へ写すプラグインを 3.2 秒の無音で眠らせ、振幅 0.8 のサイドチェーンを渡すと、`process` は 0 回、出力は 0、眠ったまま。
- 影響: サイドチェーンから音や制御を作るプラグイン（ゲインマッチ、ダッカーのメーター、サイドチェーンを聴くアナライザ、
  エンベロープから `set_param` / `midi_out` を出すもの）が、主入力が無音だと動かない。`Tail::Auto` が既定なので、何もしなくても当たる。
- 直し方の案: `busy` の判定にサイドチェーンの `is_quiet` を加える。

**現在の状態**: 第 1 回の T3 で直した。`process_with` の起床判定に、サイドチェーンの `is_quiet` を入れた（`abi.rs` の `sidechain_quiet`）。確かめたこと: `audit_fixes.rs` の `a2_sidechain_wakes_a_sleeping_instance`。この項目で残っているものは無い。

### A3. 分割した区間でトランスポートの位置が進まない（修正済み: T4）

- 場所: [abi.rs:1666](../crates/ferritron-core/src/abi.rs) `let transport = unsafe { *instance.transport.get() }.to_transport();`。区間の開始位置 `start` を使っていない。
- 実行結果: 64 サンプルのブロックを 32 で分割すると、2 回の `process` がどちらも `position_samples = Some(1000)`。
- 影響: 文書（[transport.rs](../crates/ferritron-core/src/transport.rs)）は「ブロックの先頭サンプルの位置」と言うが、プラグインから見えるブロックは区間。
  区間がホストのブロックのどこにあるかを知る方法がない。テンポ同期の LFO、シーケンサ、アルペジエータ、小節頭で動くものがずれる。
  2.0 では既定の制御周期と平滑化で分割が常に起きる（平滑化の最中は 32 サンプルごと）ので、1.4.0 より当たりやすい。
- 直し方の案: 区間ごとに `position_samples += start`、`position_beats += start / sample_rate * tempo / 60` を足す。または `ctx.block_offset()` を足す。

**現在の状態**: 第 1 回の T4 で直した。区間ごとに `position_samples` を区間の開始の分だけ進め（`abi.rs`）、区間の中の位置は `ctx.block_offset()` で分かる。確かめたこと: `a3_transport_advances_with_each_piece` と `a3_transport_stays_while_stopped`。この項目で残っているものは無い。

### A4. `"sample"` モード: ブロック末尾のパラメータ変更の後のイベント（修正済み: T1）

- 場所: [abi.rs:1787](../crates/ferritron-core/src/abi.rs)〜1801 と `ingest!` の `at.min(frames - 1) - start`（1730・1739・1771 行）。
- 内容: offset が `frames` 以上のパラメータ変更が値を変えると `run(start, frames)` の後に `start = frames` になる。続くノートや MIDI は
  `(frames - 1) - frames` を計算する。
- 実行結果（8 サンプルのブロック、offset 8 にパラメータ変更とノートオン）:
  - **debug**: `attempt to subtract with overflow` で panic。`failed` が立ち、次のブロックからも `process` は `false`（再アクティベートまで無音）。
  - **release**: panic しないが、ノートは届かない（その回も次の回も `notes = 0`）。スリープ用の `held` には入るので、そのインスタンスは眠らなくなる。
- 影響: offset がブロック長に等しいイベントは規格の外だが、他のモードはわざわざ受けている（「Events at or past the end of the block」）。
  ノートオフが消えれば鳴りっぱなしになる。
- 直し方の案: `at` を `frames - 1` に切ってから分割する。`at.min(frames - 1).saturating_sub(start)`。

**現在の状態**: 第 1 回の T1 で直した。`ingest!` は `at.min(frames - 1)` で切ってから区間の位置を引く（`abi.rs`）。確かめたこと: `a4_sample_mode_events_at_the_end_of_the_block`。この項目で残っているものは無い。

### A5. CLAP: reset と ノートの終了（修正済み: 第 2 回 A5）

- 場所: [clap_shim.c:491](../crates/ferritron-clap/shim/clap_shim.c) `plugin_reset` は空。`NOTE_END`・`NOTE_CHOKE` は `clap_shim.c` に一度も出てこない。
  `ferritron_plugin_api` に reset がない（[ferritron_abi.h](../crates/ferritron-core/include/ferritron_abi.h)）。
- 影響:
  - ホストが `reset()` を呼んでも（再生位置の移動、パニック、バウンスの開始）、ディレイや残響、鳴っている声が残る。`Process::reset` は `activate` の中でしか呼ばれない。
  - 2.0 で入れた `voice-info` と `poly = true`（N5）は、プラグインが `CLAP_EVENT_NOTE_END` を返すことを前提にする。返さないので、
    ホスト（Bitwig）は声ごとの変調をいつまでも解放できない。
  - `CLAP_EVENT_NOTE_CHOKE` が届かないので、ホストが声を切っても鳴り続ける。
  - AU の `Reset` も同じ理由で届かない（AU は読解していないが、ABI に入口がない）。
- 直し方の案: ABI に `reset`（オーディオスレッド）を足す。`ctx` から声の終了を出す道（`ctx.voice_ended(note_id, key, channel)`）を足し、CLAP で `NOTE_END` にする。
- 第 2 回 A7 で、`ctx.midi_out` と `ctx.set_param` の出力のオフセットをブロック基準にした（ホストへ渡す位置が、区間の中ではなくブロックの中になる）。

**現在の状態**: 第 1 回の T42 と第 2 回 A5 で直した。ABI の末尾に `reset` を足して CLAP の `plugin_reset` から呼び、`Process::REPORTS_VOICE_END`・`Context::voice_ended` と `FERRITRON_EVENT_NOTE_END` で CLAP の `NOTE_END` を返す（`NOTE_CHOKE` はノートオフとして渡す）。確かめたこと: `audit_fixes.rs` の `reset_clears_the_smoothing` と a5 系 4 件、CLAP validator（gain 32 合格、sine-synth 35 合格、いずれも失敗 0）。残っていること: 失敗したインスタンスではノートオフの `NOTE_END` が出ず、`REPORTS_VOICE_END = true` の経路にはテストが無く、AU の `Reset` は Mac で動かしていない。

### A6. シムのイベント上限を超えたとき（修正済み: T40（VST3・CLAP））

- 場所: [vst3_shim.cpp:832](../crates/ferritron-vst3/shim/vst3_shim.cpp)（上限 2048）、[vst3_shim.cpp:873](../crates/ferritron-vst3/shim/vst3_shim.cpp)、
  [clap_shim.c:527](../crates/ferritron-clap/shim/clap_shim.c)・591（上限 1024）。
- 内容:
  1. あふれたパラメータ変更は `set_param` で**その場で**適用する。配列に入った分は後から `process` が適用する。
     同じパラメータの点が上限をまたぐと、後の点が先に入り、前の点が後から上書きする。ブロックの終わりの値がホストの値と食い違う。
  2. ノートと MIDI は、配列がいっぱいなら何も言わずに捨てる。VST3 はパラメータを先に集めるので、密な自動化のブロックではノートオフも落ちる。
- 影響: 8 パラメータ × 512 点（`docs/performance.md` の s2 と同じ密度）で CLAP・VST3 とも上限に達する。現実のホストでは稀だが、起きたときの結果は値のずれと鳴りっぱなし。
- 直し方の案: あふれたパラメータ変更は `process` の後に適用する。ノート用の枠を別に確保する。

**現在の状態**: 第 1 回の T40 で直した。VST3 と CLAP のシムは、上限を超えたパラメータ変更を `late` の配列に入れてブロックの後に適用し、ノートの枠を別に残す（`vst3_shim.cpp` の `kMaxLate`、`clap_shim.c` の `NOTE_RESERVE`）。確かめたこと: VST3 validator（gain・sine-synth とも 537 合格・0 失敗）と CLAP validator。残っていること: 上限を超える入力を流すテストは無く、両方の配列が埋まったときの扱いは変えていない。

---

## B. 重要度が中のもの

### B1. panic を受け止めていない経路（一部修正: T30、フックの panic。第 2 回 A1・A8 で scale=custom の経路）

`AGENTS.md` は「プラグインのコードの panic は捕まえて無音にする」と言うが、`guarded` の外にプラグインのコードが残っている。
Rust は `extern "C"` を越える巻き戻しでプロセスを止めるので、ホストごと落ちる。

- `Process::migrate_state`（[abi.rs:2001](../crates/ferritron-core/src/abi.rs)、`load_state` の中）。
- `text = "custom"` の `format_param_text` / `parse_param_text`（`format_param`・`parse_param`、[abi.rs:1260](../crates/ferritron-core/src/abi.rs)〜）。
- `scale = "custom"` の `param_normalize` / `param_denormalize`: `get_param`・`set_param`・`to_plain`・`to_normalized`、
  そして `process_with` の起床判定のループ（[abi.rs:1601](../crates/ferritron-core/src/abi.rs)、`guarded` の前）と失敗時の `apply`。
- `on_notify` のハンドラ（`take_requests` → `deliver_notifications`、[abi.rs:276](../crates/ferritron-core/src/abi.rs)）と `on_message` のハンドラ。

第 2 回 A8 で、`instance.rs` の `normalize` / `denormalize`（`notify`、`normalized`、`set_normalized`）も、panic を受け止めるようにした。

**現在の状態**: 第 1 回の T30 と第 2 回の A1・A8 で直した。`migrate_state`、`format`・`parse` の文字列、`scale = "custom"` の起床判定と適用（`denormalize_or`・`normalize_or`）、通知とメッセージのハンドラ（`deliver_notifications` の `catch_unwind`）は、いずれも `guarded` か `catch_unwind` の中にある（コードを grep で確かめた）。確かめたこと: `a1_a_panicking_custom_scale_stays_inside_the_plugin_call`、`a8_instance_handle_conversions_survive_a_custom_scale_panic`、`a_panicking_migration_does_not_reach_the_host`。残っていること: 文字列の処理と通知のハンドラで panic を起こすテストは無い。

### B2. 組み込みバイパス（一部修正: T8、プールのバッファが返らない件。第 2 回 A2 で、完全バイパスの後の reset を追加）

- 場所: [bypass.rs](../crates/ferritron-core/src/bypass.rs)、[abi.rs:1645](../crates/ferritron-core/src/abi.rs)・1906〜。
- **プールのバッファが返らない**: `begin` がプールのバッファを借りた後、`plugin.process` が panic すると `finish` を呼ばない（`None` の枝）。
  `busy` は立ったまま。4 回起きると、そのプロセスのどのインスタンスもクロスフェードできなくなる。
- **完全バイパス中はプラグインを呼ばない**（`skip_plugin`）。そのため:
  - バイパス前に鳴らしたノートのノートオフが届かない。解除後に鳴りっぱなしになる。
  - ディレイや残響の中身が凍り、解除したときに古い音が出る。`reset` も呼ばない。
  - 平滑化と `changed()` の状態も止まる。
- 主バスのチャンネル数が `set_layout` と合わないとき（`layout` が空なら 2 と仮定）、余ったチャンネルはレイテンシ分の遅延がかからない。

**現在の状態**: 第 1 回の T8 で、panic のあともプールのバッファが返るようにした。第 2 回 A2 で、完全バイパスを解いた最初のブロックの前に `Process::reset` を呼ぶようにした（`bypass.rs` の `take_resume`、`abi.rs` の `process_with`）。確かめたこと: `audit_fixes.rs` の `b2_unbypass_resets_the_plugin`。残っていること: 主バスのチャンネル数が `set_layout` と違うとき、余ったチャンネルにレイテンシ分の遅延がかからない件は直していない。

### B3. 終わらないスレッドと DLL のアンロード（一部修正: 第 2 回 A3。DLL を固定した。スレッドは止まらないまま）

- `ferritron-notify`（[instance.rs:942](../crates/ferritron-core/src/instance.rs)、`on_notify` を使うと始まる）と
  `ferritron-watch`（[watch.rs:133](../crates/ferritron-core/src/watch.rs)、`instances_watch` を使うと始まる）は無限ループで、止める道がない。
- `worker.rs` は最後のインスタンスで join し、`parallel.rs` は DLL を固定している。この 2 本にはどちらもない
  （`pin_current_module` は Unix 用だけで、呼ぶのは `parallel.rs` だけ）。
- 影響: ホストが最後のインスタンスを消してプラグインの DLL をアンロードすると、眠っているスレッドが戻る先のコードがなくなり、落ちる可能性がある。
  実行では確かめていない。

**現在の状態**: 第 2 回 A3 で、DLL を固定してからスレッドを始めるようにした（`module.rs` の `pin_current_module`、`instance.rs` の `start_notify_fallback`、`watch.rs` の `start`）。固定できなければスレッドは始めない。確かめたこと: `cargo test -p ferritron-core` と `cargo test -p midi-out`。固定の効果（ホストが DLL を外す場面）は確かめていない。残っていること: スレッドは止まらないまま（止める道は作っていない）。

### B4. 通知のフォールバックが別スレッドでハンドラを呼ぶ（修正済み: T23）

- 場所: [instance.rs:932](../crates/ferritron-core/src/instance.rs)〜（`POLL_GRACE_MS = 100`）。
- 内容: シムが 100 ms ポーリングしないと「ポーリングしない形式」と見なし、`ferritron-notify` がハンドラを呼ぶ。
  VST3・VST2・AAX はメインスレッドのタイマーが 30 ms ごとにポーリングするが、メインスレッドが 100 ms 止まれば（モーダルなダイアログ、
  プロジェクトの読み込み、多くのホストのバウンス）条件を満たす。
- 影響: 「ホストのメインスレッドで呼ぶ」という `on_notify` の約束が破れる。`apply_program`（プリセットの読み込み）も同じスレッドで走る。

**現在の状態**: 第 1 回の T23 で直した。フォールバックのスレッドは、一度もポーリングしないシム（LV2・スタンドアロン・AU など）のインスタンスだけを対象にし、ポーリングしたことのあるシムのインスタンスはメインスレッドで配る（`instance.rs` の `start_notify_fallback` と `polled` の確認）。確かめたこと: `cargo test -p ferritron-core` と `midi-out` の fallback のテスト。残っていること: 一度もポーリングしないシムには、今も別スレッドから配る。

### B5. 平滑化と `ctx.changed()`（修正済み: T2）

- 場所: [plugin.rs:549](../crates/ferritron-core/src/plugin.rs)、[smooth.rs](../crates/ferritron-core/src/smooth.rs)。
- 実行結果: 20 ms の平滑化で値を 2 → 4 に変えると、`changed = true` は最初の 1 回だけ。そのときの `value()` はまだ 2.000（区間の先頭の値）。
  以後の区間は `changed = false` のまま `value()` が動く。
- 影響: `if ctx.changed(&p) { 係数を計算 }` と書くと、古い値で 1 回計算して終わる。`docs/migration-2.0.md` はこの組み合わせに触れていない。
- 直し方の案: 平滑化の最中は `changed()` を真にする。または文書に「平滑化したパラメータは `is_smoothing` も見る」と書く。
- 関連: `FloatParam::value()` はオーディオスレッドが書いた平滑化後の値を返す。エディタのスレッドから呼ぶと、処理が止まっている間は古い値のまま。

**現在の状態**: 第 1 回の T2 で直した。`Context::changed()` は、平滑化の途中で値が動いた区間で真を返す（`plugin.rs` の `changed` が `shown` を比べる）。確かめたこと: `audit_fixes.rs` の `b5_changed_follows_the_smoothed_value`。この項目で残っているものは無い。

### B6. VST3 のメインスレッドのポーリング（修正済み: 第 2 回 C1 の Windows。Linux は文書に記載: 第 2 回 C5）

- 場所: [vst3_shim.cpp:405](../crates/ferritron-vst3/shim/vst3_shim.cpp) `SetTimer(nullptr, 0, 30, tick)`。
- ウィンドウのないスレッドタイマーは、作ったスレッドがメッセージを回すときだけ動く。最初のインスタンスをメインスレッド以外で作るホストでは
  動かず、プラグイン自身の編集の通知、レイテンシの変更、`kParamValuesChanged` が届かない。`KillTimer` も別スレッドからは効かない。
- Linux（`_WIN32` でも `__APPLE__` でもない）: `pollMain` を呼ぶものがない。`request_latency` と `load_values` の通知は届かない。
  `push` はロックを持ったまま、呼んだスレッドで `performEdit` を呼ぶ（VST3 は UI スレッドを求める）。
- `drain`: `pending` をキューから抜いた後で `pollMain` → `restartComponent` を呼ぶ。その中でホストがインスタンスを消すと、
  `pending` に残った編集が解放済みの `Plugin*` を指す（[vst3_shim.cpp:1451](../crates/ferritron-vst3/shim/vst3_shim.cpp)）。
- `setActive(true)` の中から `restartComponent(kLatencyChanged)` を呼ぶ。再入を嫌うホストがある。

**現在の状態**: 第 2 回 C1 で Windows の部分を直した。`drain` は各呼び出しの前に `alive()` で登録を確かめ、Windows では `setActive` の `kLatencyChanged` を `latencyPending` に置いて次の `pollMain` で送る（`vst3_shim.cpp`）。確かめたこと: VST3 validator 537 合格・0 失敗と、`ferritron-testhost` の 5 秒の実行。残っていること: Linux には `pollMain` を呼ぶ処理がまだ無く（README に記載）、macOS は `setActive` の中で直接呼ぶまま、VST3 のスレッドタイマーは対象外。

### B7. 状態の保存の 2 回呼び出し（修正済み: T41）

- 場所: [vst3_shim.cpp:752](../crates/ferritron-vst3/shim/vst3_shim.cpp)、[clap_shim.c:942](../crates/ferritron-clap/shim/clap_shim.c)、
  [vst2_shim.cpp:499](../crates/ferritron-vst2/shim/vst2_shim.cpp)。
- 内容: 1 回目でサイズを聞き、2 回目で書かせる。その間に別スレッドがストア（`InstanceHandle::store`）を大きくすると、
  `save_state` は「入らない」と判断して何も写さず、シムはゼロのままのバッファを保存する。そのプロジェクトは読み込みに失敗する。
  戻り値を見ていない（LV2・AAX は見てエラーを返す。AU は `written.min(size)` で切るだけなので同じくゼロを保存する）。
- 直し方の案: 戻り値がバッファより大きければやり直す。

**現在の状態**: 第 1 回の T41 で直した。VST3・CLAP・VST2・AU の状態の保存は、サイズを聞いてから書かせ、戻り値がサイズを超えたらやり直す（3 回まで。だめなら保存を失敗させる）。確かめたこと: VST3 validator 537 合格・0 失敗。残っていること: VST2 と AU は実行して確かめていない（AU は Mac で動かしていない）。

### B8. プラグインからの出力イベント（修正済み: f3・f4・f9）

- `out_events` を渡すのは VST3 と CLAP のシムだけ。VST2・LV2・AU・AAX・スタンドアロンは渡さない。
  隔離のクライアント（[client.rs:630](../crates/ferritron-core/src/isolation/client.rs)）は受け取っても子プロセスへ回さない。
- 影響: `ctx.midi_out` と `ctx.set_param` のホストへの通知が、これらでは黙って消える（`dropped` に数えるだけで、読む公開 API がない）。
  LV2 と AU は形式としては MIDI を出せる。

**現在の状態**: 直した。`midi_out` と `set_param` の出力は、VST3 と CLAP に加えて、VST2（ホストが受け取りを名乗ったとき）、LV2（MIDI だけ）、スタンドアロン（MIDI の出力先を選んだとき）、AU（MIDI とパラメータの通知。Mac では未実行）、WebAssembly と隔離（子プロセスの出力を親へ返す）で送るようになった（`f3-report-*.md`・`f4-report-*.md`）。残っていること: LV2 はパラメータの変更を送らず、AAX はパラメータの変更（f4）と MIDI 出力（f9、出力の MIDI ノード）を送る。AAX は SDK 2.10 でコンパイルとリンクまでで、Pro Tools では動かしていない（`f9-report-1.md`）。

### B9. `[vst3] controller_id`（文書に記載: 第 2 回 C5。修正済み: f4、編集画面の経路は未確認）

- 場所: [vst3_shim.cpp:1947](../crates/ferritron-vst3/shim/vst3_shim.cpp)〜。コントローラのクラスも `api->create()` で完全なインスタンスを作る。
- 影響: 1 つのプラグインにつき `instances::<P>()` に 2 つ並ぶ。ワーカー、MCP の登録、エディタの先読みも 2 つ。
  エディタはコントローラ側のインスタンスにつながるので、`[streams]`（メーター）、`store`、`on_message` はプロセッサ側に届かない。
  コントローラの状態は保存しない（`getControllerState` は空）ので、エディタが `store` に書いたものは消える。

**現在の状態**: f4 の 3-1 で直した。コントローラの写しは `instances` と MCP の一覧から外し（`abi.rs` の `set_role`、`instance.rs` の `Link::hide`）、ホストが同じプロセスでつなげば VST3 のプロセッサとコントローラが `IConnectionPoint` で接続する（`vst3_shim.cpp`）。確かめたこと: `controller_copy_is_hidden_from_the_instance_lists`、VST3 validator（`vst3-ids` を含む 3 本とも 537 合格）、使い捨てのホストでの接続の確認。残っていること: 編集画面の経路（`[view]` と `controller_id` を両方持つプラグイン）は動かして確かめていない。コントローラの状態は保存しない。

### B10. 隔離のタイムアウト（修正済み: 第 2 回 B3、f1 の 2-2）

- 場所: [shm.rs:11](../crates/ferritron-core/src/isolation/shm.rs) `PROCESS_TIMEOUT_MS = 250`、[client.rs:382](../crates/ferritron-core/src/isolation/client.rs)。
- `Setup::offline` の文書は「オフラインでは `process` は好きなだけ時間をかけてよい」と言うが、隔離では 250 ms で子プロセスを止めて無音にする。
  最初のブロックでの確保や GPU の準備、デバッガでの停止でも当たる。
- パイプライン モード: `VecDeque` に 1 サンプルずつ入れて出す。ブロックが `latency` を超えると容量を超えてオーディオスレッドで確保する。

**現在の状態**: 第 2 回 B3 と f1 の 2-2 で直した。`isolation/client.rs` の `timeout_ms()` が、オフラインでは `OFFLINE_TIMEOUT_MS`（30 秒）、`activate` の後の最初のブロックでは `FIRST_BLOCK_TIMEOUT_MS`（5 秒）、それ以外は `PROCESS_TIMEOUT_MS`（250 ms）を使う（`shm.rs`）。オフラインは `remote_set_offline` で子プロセスへも伝え、パイプラインの容量は `latency + max_block + 1` にした（`debug_assert` で確かめる）。確かめたこと: `cargo test -p ferritron-core isolation` と `the_child_passes_the_offline_flag_to_the_plugin_on_change_only`。残っていること: 子プロセスの `Quality::Best` になるかは確かめていない。

### B11. 状態の読み込みと既定値（修正済み: T6）

- 場所: [state.rs:47](../crates/ferritron-core/src/state.rs)。状態にある ID だけを書く。
- 影響: パラメータが増える前の状態を、値を動かした後のインスタンスに読ませると（ホストのプリセット切り替え）、新しいパラメータは
  既定値ではなく今の値のまま残る。読み込む順で結果が変わる。

**現在の状態**: 第 1 回の T6 で直した。状態に無いパラメータは既定値に戻す（`state.rs`）。確かめたこと: `audit_fixes.rs` の `b11_a_parameter_missing_from_the_state_gets_its_default`。この項目で残っているものは無い。

---

## 性能

`docs/performance.md` に既に書いてあるものには「既知」と付けました。

| # | 内容 | 場所 |
|---|---|---|
| P1 | **平滑化の最中は制御周期（32 サンプル）ごとに `process` を呼ぶ。** 実行: 64 サンプル × 3 ブロックで 5 回。`exp` は目標の 1e-9 以内になるまで動いていると見なすので、時定数の約 20 倍続く（20 ms なら約 0.4 秒）。平滑化したパラメータを自動化し続けると、ずっと分割したまま | [abi.rs:1837](../crates/ferritron-core/src/abi.rs)、[smooth.rs:134](../crates/ferritron-core/src/smooth.rs) |
| P2 | 既知: s3〜s5 は 1.4.0 より 2〜10% 遅い。密な自動化（s2）は 67 µs で、目標（自動化なしの 2 倍以内）に届いていない | `docs/performance.md` |
| P3 | 毎ブロック、主入力の全サンプルを `is_quiet` で走査する（音があれば最初の 64 サンプルで抜ける。無音のときは全部）。`Tail::Auto` で入力が無音の間は出力も走査 | [abi.rs:984](../crates/ferritron-core/src/abi.rs)・1619・1911 |
| P4 | レイテンシのあるプラグインは、バイパスしていなくても毎ブロック、全チャンネルを 1 サンプルずつ遅延リングへ写す（分岐つきのスカラーのループ。`copy_from_slice` 2 回にできる） | [bypass.rs:164](../crates/ferritron-core/src/bypass.rs) |
| P5 | `"events"` モードは毎ブロック `ParamEvent` 1024 個（24 KB）をゼロで埋める。ノートの配列で直したのと同じ種類（約 1 µs） | [abi.rs:1804](../crates/ferritron-core/src/abi.rs) | （修正済み: T9）
| P6 | オーディオスレッドのスタック: VST3 シムの `events[2048]`（49 KB）+ 整列用（49 KB）+ コアのノート・MIDI・声・パラメータの配列（約 70 KB）で約 170 KB。スタックの小さいスレッドで呼ぶホストでは余裕が少ない | [vst3_shim.cpp:817](../crates/ferritron-vst3/shim/vst3_shim.cpp)・937、[abi.rs:1678](../crates/ferritron-core/src/abi.rs) |
| P7 | 毎ブロック `bulk` の `Mutex::try_lock`（メインスレッドが待っていれば、解放で起こすためのシステムコール）。`AtomicBool` を先に見れば済む。パラメータのイベントは、起床判定と適用で 2 回 `lookup` と変換をする | [instance.rs:370](../crates/ferritron-core/src/instance.rs)、[abi.rs:1601](../crates/ferritron-core/src/abi.rs) | （修正済み: T5（P7 の bulk 部分））
| P8 | `parallel_for` のプールは同時に 1 ジョブ。他のインスタンスは待たずに 1 スレッドで実行するので、`parallel = true` のインスタンスが並ぶと大半が直列になる。呼び出し側は終わるまでスピンし、ワーカーが割り込まれるとその間ずっと回る。ブロックごとに `unpark`（システムコール） | [parallel.rs:247](../crates/ferritron-core/src/parallel.rs) |
| P9 | CLAP: `params_get_value` が毎回すべてのパラメータの `param_info` を引く（全パラメータの読み直しで O(n²)）。`report_edits` はオーディオスレッドでスピンロックを取る（UI スレッドがロック中に割り込まれると回り続ける） | [clap_shim.c:899](../crates/ferritron-clap/shim/clap_shim.c)・330 |
| P10 | `Oversampler` はマスクつきのリングを 1 サンプルずつ読むスカラーの FIR。T7 で測った `Biquad` のような SIMD 版がない（改善の余地。測っていない） | [oversampler.rs](../crates/ferritron-dsp/src/oversampler.rs) |
| P11 | 既知: Web エディタの先読みは固定で約 230 MB。隔離の往復は平均 7.9 µs / p99 13.5 µs（カーネルの待ちを含む） | `docs/performance.md` |

**現在の状態**（性能の P。`docs/performance.md` の方針どおり測ってから直す。この回の範囲外）:

- **P1** 未修正。平滑化の最中に制御周期ごとに分けて呼ぶ作りは変えていない。
- **P2** 既知のまま。`docs/performance.md` に書いてある。
- **P3** 未修正。
- **P4** 未修正。
- **P5** 修正済み（第 1 回の T9）。
- **P6** 未修正。VST3 シムの配列の大きさは今のまま。
- **P7** 一部修正（第 1 回の T5 で `bulk` の確認を `AtomicBool` で先に行う）。パラメータの変換を二重に行う件は未修正。
- **P8** 未修正。
- **P9** 未修正。
- **P10** 未修正。
- **P11** 既知のまま。`docs/performance.md` に書いてある。

---

## 小さな問題

- **C1** `Context::changed` は `seen.state()` の `&mut` を持ったまま `seen.index()` でもう 1 つ `&mut` を作る（[plugin.rs:549](../crates/ferritron-core/src/plugin.rs)）。 （修正済み: T2）
  形式上は未定義動作（同じデータへの 2 つの `&mut`）。実害は見ていない。

  **現在の状態**: 第 1 回の T2 で、`changed` は `seen.index()` を先に呼び、`state()` の `&mut` を重ねないようにした（`plugin.rs`）。実害は見ていなかった項目で、この項目で残っているものは無い。
- **C2** `ctx.midi_out` はキーやコントローラ番号を 0..127 に切らない。`ctx.set_param` のイベントは offset が常に 0（[plugin.rs:665](../crates/ferritron-core/src/plugin.rs)〜）。 （修正済み: T11）

  **現在の状態**: 第 1 回の T11 で、`midi_out` の鍵とコントローラ番号を 127 までに切った（`plugin.rs`）。`set_param` の offset は第 2 回 A7 でブロック基準にした。確かめたこと: `audit_fixes.rs` の `a7_midi_out_offsets_are_block_offsets`。この項目で残っているものは無い。
- **C3** `check_ids` の予約 ID の表に `ferritron.program`（ハッシュの ID）と VST3 の MIDI コントローラ用の隠しパラメータの範囲
  （`0x40001000`〜、4096 個）がない（[manifest lib.rs:1268](../crates/ferritron-manifest/src/lib.rs)、[vst3_shim.cpp:63](../crates/ferritron-vst3/shim/vst3_shim.cpp)）。 （修正済み: 第 2 回 C3）
  ハッシュの ID は 31 ビットなので、この範囲に入る確率はパラメータ 1 個あたり約 200 万分の 1。当たると、そのパラメータの自動化が MIDI になる。

  **現在の状態**: 第 2 回 C3 で直した。`check_ids`（`ferritron-manifest/src/lib.rs`）の予約 ID に `reserved_id("ferritron.program")` と VST3 の MIDI の範囲 `0x4000_1000..0x4000_2000` を足した。確かめたこと: `program_and_midi_controller_ids_are_reserved`（`cargo test -p ferritron-manifest` で 37 件合格）。この項目で残っているものは無い。
- **C4** スリープ用の「押されているノート」は、ノートオフでしか消えない。All Notes Off（CC 123）や、ノートオフを送らずに止めるホストでは、
  再アクティベートまで眠らない。CLAP の `key = -1`（`note_id` だけのノートオフ）は捨てる。（一部修正: 第 2 回 A4。CC 120・123 のみ。CLAP の `key = -1` は未修正）

  **現在の状態**: 第 2 回 A4 で、CC 120（All Sound Off）と 123（All Notes Off）が、そのチャンネルの押されているノートの記録（`abi.rs` の `Sleep` の `held`）を消すようにした。確かめたこと: `c4_all_notes_off_lets_the_plugin_sleep`。残っていること: CLAP の `key = -1` のノートオフは今も捨てる。
- **C5** `InstanceHandle::wake` の文書は「`request_process` がなければロックなし」と言うが、常に `host` の `Mutex` を取る（[instance.rs:517](../crates/ferritron-core/src/instance.rs)）。 （修正済み: T11）

  **現在の状態**: 第 1 回の T11 で、`InstanceHandle::wake` の文書を実際の動き（常に `host` のロックを取る）に合わせた。コードは変えていない。
- **C6** VST2: `audioMasterNeedIdle` に頼る。多くのホストは応えないので、エディタを閉じているとレイテンシの変更と値の変更が届かない。

  **現在の状態**: 未修正。`audioMasterNeedIdle` に頼る作りは変えていない（この回では扱っていない）。
- **C7** `cargo clippy --all-targets` は `ferritron-dsp` のテストで止まる（`design.rs:134`・138 の `0.7071` が deny の `approx_constant`）。 （修正済み: T24/T25）
  `ferritron-core` には警告が 5 件（`fetch_update` の置き換え、使っていないフィールドなど）。

  **現在の状態**: 第 1 回の T24 で `ferritron-dsp` の `design.rs` の定数を直し、`cargo clippy -p ferritron-dsp --all-targets` でエラーが出なくなった。残っていること: `ferritron-core` の警告（`instance.rs` の `fetch_update` など）は残っている（f4 の報告で確かめた）。
- **C8** `DelayLine::read_frac` に NaN を渡すと、`clamp` が NaN を返し、出力が NaN になる（[delay.rs:50](../crates/ferritron-dsp/src/delay.rs)）。 （修正済み: T24/T25）

  **現在の状態**: 第 1 回の T24 と T25 で、`DelayLine::read_frac` の NaN を扱うようにし、`ferritron-dsp` に NaN のテストを足した。この項目で残っているものは無い。
- **C9** パラメータのイベントを眠ったまま適用する経路（`apply`）は、値が同じでも `bump_params` を呼ぶ。同じ値を毎ブロック送るホストでは （修正済み: T7）
  `params_revision` が増え続け、`instances_watch` が毎回「変わった」と見る（[abi.rs:1552](../crates/ferritron-core/src/abi.rs)）。

  **現在の状態**: 第 1 回の T7 で、眠ったまま同じ値を送られても `bump_params` を呼ばないようにした（`abi.rs` の `apply`）。確かめたこと: `c9_resent_values_while_asleep_move_nothing`。この項目で残っているものは無い。
- **C10** `Process::migrate_state` は、保存した版のほうが新しいとき（古いプラグインで新しいプロジェクトを開く）も呼ぶ。文書は古い版からの移行だけを説明している。 （修正済み: T11）

  **現在の状態**: 第 1 回の T11 で文書を直した。`migrate_state` は、保存した版が新しいとき（`from > to`）も呼ばれると書いた（`plugin.rs`）。コードの動きは変えていない。

---

## 直す順の提案

1. A1・A2・A3・A4: コアの数行〜数十行で直り、`Harness` のテストで固定できる。
2. A6・B7: シムだけの変更。
3. A5: ABI に入口を足す（v2 の末尾に足せば互換）。
4. B1・B2（バッファの返却）・B3: 落ちる種類のもの。
5. P1・P4・P5・P7: 測ってから（`ferritron-bench-fx`）。
