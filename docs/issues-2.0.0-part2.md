# Ferritron 2.0.0 の問題点の洗い出し（続き）

2026-11-10、ブランチ `ferritron-2.0`（`c72816c`）。[issues-2.0.0.md](issues-2.0.0.md) の続きで、そちらで「読んでいない」とした範囲を調べた記録です。
番号は前の文書と重ならないように D〜 から始めます。修正はしていません。

## 今回調べた範囲と方法

- **読んだもの**: `ferritron-wasm` のホスト側（インスタンス、`process`、状態）、`ferritron-lv2` の `lib.rs`（全体）、
  `ferritron-standalone` の `engine.rs`、`ferritron-core` の `stream.rs`・`voice.rs`・`settings.rs`・`mcp.rs`・`util.rs`（`Swap`）、
  `ferritron-web` の `imp.rs`（ビュー、ブリッジ、メッセージ）、`ferritron-dsp` の `lookahead.rs`・`smoother.rs`、`tools/ferritron-mcp` の接続部分。
- **実行して確かめたもの**: 一時的な結合テスト（実行後に削除済み）で 2 件。前の文書の B2（バイパス中のノートオフ）と B11（状態にないパラメータ）で、
  どちらも読解のとおりだった（下の「前の文書への追記」）。
- **まだ読んでいないもの**: AU / AUv3 / AAX の各アダプタ、`ferritron-gpu-view`、`ferritron-gpu` / `-graph` / `-slang` / `-vulkan`、
  `ferritron-native-graphics`、`ferritron-ara`、`ferritron-cli`、プロジェクトロック（非公開）、ホットリロード、隔離のサーバー側とビュー、
  `ferritron-dsp` の `biquad.rs`・`fft.rs`・`simd` など。**これらに問題がないという意味ではありません。**
- この文書の項目は、断りがなければコードを読んで確かめただけで、実行していません。

## 一覧

| # | 重要度 | 内容 |
|---|---|---|
| D1 | 高 | WebAssembly 隔離: 1 つの `Store` をオーディオスレッドと UI スレッドが同時に使う（排他なし） |
| D2 | 高 | LV2: 2 バイトの MIDI（プログラムチェンジ、チャンネルプレッシャー）が届かない |
| D3 | 高 | LV2: トランスポートの位置が、ホストが位置を送り直すまで止まったまま |
| D4 | 中 | `VoiceMods`: ノートオフで声の変調と表現を捨てる（リリース中に値が飛ぶ）。同じキーの重なりを区別できない |
| D5 | 中 | MCP: インスタンスを消すとき、エージェントのスレッドが解放済みのインスタンスを読みうる |
| D6 | 中 | Web エディタ: ページの移動先を制限していない。外部のページにも `window.ferritron` が入る |
| D7 | 中 | `StreamWriter::write` / `TapWriter::write` は `&self` で `Sync`。安全なコードから 2 スレッドで呼べる |
| D8 | 中 | 設定ファイル: 読めないファイルを空として扱い、次の書き込みで上書きする |
| D9 | 中 | LV2: プリセット（`load_values`）がホストのコントロールポートと食い違い、保存して開き直すと消える |
| D10 | 中 | スタンドアロン: ノート以外の MIDI を捨てる。トランスポートなし、ステレオ固定 |
| D11 | 中 | MCP: プラグイン側が、ポートで待つ相手を確かめずにトークンを送る |
| Q1〜Q8 | 性能 | 下の「性能」の章 |
| E1〜E9 | 低 | 下の「小さな問題」の章 |

---

## D. 問題点

### D1. WebAssembly 隔離の `Store` に排他がない（修正済み: 第 2 回 B4）

- 場所: [ferritron-wasm lib.rs:526](../crates/ferritron-wasm/src/lib.rs) `unsafe fn instance<'a>(p) -> &'a mut Instance`。
  `get_param`（561 行）、`set_param`（567 行）、`process`（641 行）、`save_state`（717 行）、`load_state`（728 行）がすべてこれを使い、
  同じ `Sandbox`（wasmtime の `Store`）でゲストを呼ぶ。
- 内容: ホストは `get_param` / `set_param` を UI スレッドから、`process` をオーディオスレッドから呼ぶ。Web エディタは 33 ms ごとに
  全パラメータの `get_param` を呼ぶ（[imp.rs:183](../crates/ferritron-web/src/imp.rs)）。`Store` は 1 スレッド用で、ロックもない。
  同じインスタンスへの `&mut` が 2 つでき、ゲストのメモリとランタイムの状態を 2 スレッドが同時に書く。
- 影響: エディタを開いて再生するだけで条件がそろう。結果はゲストの中の不整合では済まず、ホスト側の未定義動作（隔離の目的に反する）。
  実行では確かめていない。
- 関連: `to_plain` / `to_normalized` / `format_param` は全インスタンス共有の `meta` の `Mutex` を取ってゲストを呼ぶ（573 行）。
  CLAP シムは `process` の中でパラメータのイベントごとに `to_normalized` を呼ぶ（[clap_shim.c:526](../crates/ferritron-clap/shim/clap_shim.c)）ので、
  オーディオスレッドが UI スレッドと同じロックを待つ。
- 直し方の案: パラメータの値はホスト側の原子的な写しに置き（プロセス隔離の `mirror` と同じ形）、ゲストを呼ぶのはオーディオスレッドだけにする。
  変換（`to_plain` など）は読み込み時にゲストから表を取り出してホスト側で計算する。

**現在の状態**: 第 2 回 B4 で直した。ゲストは `Mutex<Live>` の中で 1 スレッドずつ呼び、パラメータの読み書きはホスト側の `values` と `dirty` の写しで行う（`ferritron-wasm/src/lib.rs`）。`process` は `try_lock` で、取れなければそのブロックを無音にする。確かめたこと: `cargo build -p ferritron-wasm` と `cargo build -p wasm-sandbox`（テストは無い）。残っていること: CLAP シムが `process` の中で `to_normalized` を呼ぶ件（`meta` のロック）は直していない。

### D2. LV2: 2 バイトの MIDI が届かない（修正済み: T20）

- 場所: [ferritron-lv2 lib.rs:301](../crates/ferritron-lv2/src/lib.rs) `event.body.size >= 3`。
- 内容: プログラムチェンジ（0xC0）とチャンネルプレッシャー（0xD0）は 2 バイト。この条件で落ちる。すぐ下の
  `bytes.get(2).copied().unwrap_or(0)` は 2 バイトを受けるつもりの書き方だが、その前に `from_raw_parts(data, 3)` で 3 バイト読んでいる。
- 直し方の案: `size >= 2` で受け、3 バイト目は `size >= 3` のときだけ読む。

**現在の状態**: 第 1 回の T20 で直した。`size >= 2` で受けるようにした（`ferritron-lv2/src/lib.rs`）。確かめたこと: `cargo build -p gain`。LV2 のホストでは確かめていない（LV2 のテストは無い）。

### D3. LV2: トランスポートの位置が進まない（修正済み: T21）

- 場所: [ferritron-lv2 lib.rs:319](../crates/ferritron-lv2/src/lib.rs)〜（`time_events`）、438 行。
- 内容: LV2 のホストは `time:Position` を変化があったときだけ送る（再生中に毎ブロック送るとは限らない）。シムは受け取った値を
  `me.transport` に置いたままにし、次のブロックでも同じ `position_samples` / `position_beats` を渡す。`time:speed` と `frames` から進めていない。
  イベントの `frames`（ブロック内の位置）も見ていない。
- 影響: LV2 でテンポ同期のプラグインがずれる。前の文書の A3（区間ごとに進まない）と重なると、さらにずれる。
- 直し方の案: 再生中（`speed > 0`）は、ブロックの終わりに `position_samples += frames * speed`、拍の位置もテンポから進める。

**現在の状態**: 第 1 回の T21 で直した。`time:Position` を受けた後は、`advanced()` が再生中（`speed > 0`）の位置をブロックごと・区間ごとに進める（`ferritron-lv2/src/lib.rs`）。確かめたこと: `cargo build -p gain`。LV2 のホストでは確かめていない。

### D4. `VoiceMods` はノートオフで声を忘れる（修正済み: 第 2 回 A5）

- 場所: [voice.rs:187](../crates/ferritron-core/src/voice.rs)（`note_off` が `EMPTY` にする）、255・302 行（`feed_note` / `feed` がノートオフで呼ぶ）。
- 内容:
  - ノートオフの時点で、ホストの変調（`host_mods`）、ノートエクスプレッション、プラグイン自身のオフセットを消す。
    シンセの声はリリースの間まだ鳴っているので、`value_for` がその瞬間に基準値へ飛ぶ。CLAP では変調は `NOTE_END` まで有効。
  - 声の識別はチャンネルとキー（`VoiceId`）。同じキーを重ねて鳴らすと前の声の記録を上書きする。一方で CLAP シムは
    `CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES` を名乗る（[clap_shim.c:1459](../crates/ferritron-clap/shim/clap_shim.c)）。
  - `feed` で入れた声は `note_id` が -1 のまま。`note_id` つきのイベント（VST3 のノートエクスプレッションは key・channel が -1）が来ると、
    284 行の条件がすべての声に当たる。
  - 1 声あたり 8 パラメータ（`MAX_MODS_PER_VOICE`）、64 声（`MAX_VOICES`）を超えた分は何も言わずに捨てる。
- 直し方の案: 「離した」と「鳴り終わった」を分ける（`release(id)` と `end(id)`）。前の文書の A5（`NOTE_END` を返す道）と一緒に入れる。

**現在の状態**: 第 2 回 A5 で直した。`VoiceMods` は、ノートオフで枠を消さず `released` にし、`end()` で消す（`voice.rs`）。CLAP は重なりを名乗らない（`voice_info` の flags は 0）。確かめたこと: `a5_a_released_voice_keeps_its_offsets_until_end`、`a5_released_voices_are_reused`、`a5_a_note_id_alone_reaches_only_its_note`。残っていること: 同じキーの重なりは、チャンネルとキーの識別のままなので区別できない。

### D5. MCP: 消したインスタンスをエージェントが読みうる（修正済み: 第 2 回 B1。unregister の待ちを確かめるテストは無い）

- 場所: [mcp.rs:172](../crates/ferritron-core/src/mcp.rs)（`unregister`）、201 行〜（`run`）、[abi.rs:1029](../crates/ferritron-core/src/abi.rs)（`Endpoint` は生ポインタを持つ）。
- 内容: `unregister` は一覧から外すだけで、スレッドを止めて待つのは最後の 1 つのときだけ。エージェントのスレッドは、その周回で取った
  `enabled`（`Arc<dyn Endpoint>` の写し）を持ったまま `serve` を呼ぶ。`serve` → `parameters()` / `set()` は `&*self.instance` を読む。
  AI Access を入れたインスタンスが 2 つ以上あり、要求の処理中に片方を消すと、解放済みの `Instance` を読む。
- 直し方の案: `unregister` が、そのエンドポイントを使う呼び出しが終わるまで待つ（エンドポイントごとの `Mutex` か世代番号）。
- 関連: キャプチャ用のリングは最初に AI Access を入れたときのサンプルレートで作り、作り直さない（118 行）。レートが変わると WAV のヘッダと長さがずれる。
  WAV は一時フォルダの `ferritron-mcp` に増え続け、消すものがない（295 行）。

**現在の状態**: 第 2 回 B1 で直した。`unregister` は、そのインスタンスを使っている要求が終わるまで待ち（`mcp.rs` の `alive` の書き込みロック）、消えた後の要求はエラーで返す（`while_alive`）。キャプチャは `ensure_tap` でサンプルレートが変わると作り直し、1 時間より古い WAV を消す。確かめたこと: `cargo test -p ferritron-core mcp`（既存のテストが通る）。残っていること: `unregister` の待ちを確かめるテストは無い（`Shared` が `Link` を要るため、単体では組めなかった）。

### D6. Web エディタ: 移動先の制限がない（修正済み: T29）

- 場所: [imp.rs:316](../crates/ferritron-web/src/imp.rs)〜。`with_navigation_handler` も新しいウィンドウの扱いもない（`ferritron-web` に一度も出てこない）。
- 内容: ブリッジは `with_initialization_script` で入れるので、移動した先のページにも入る。エディタの HTML にあるリンクや
  `location` の書き換えで外部のページへ移ると、そのページが `window.ferritron` と IPC を使える: パラメータの変更、設定ファイルへの書き込み、
  ファイルのダイアログ、プラグインへのメッセージ。
- 影響: プラグインの作者が外部のコンテンツ（マニュアル、ライセンスのページ、広告）を同じビューで開いたときに起きる。
- 直し方の案: `ferritron.localhost`（WebKit では `ferritron:`）以外への移動は既定のブラウザで開き、ビューの中では拒む。

**現在の状態**: 第 1 回の T29 で直した。移動先を `ferritron.localhost`、`ferritron:`、`about:blank` に限り、新しいウィンドウは開かない（`ferritron-web/src/imp.rs` の `with_navigation_handler`、`with_new_window_req_handler`）。確かめたこと: `cargo build -p gain`（web 機能つき）。残っていること: 外部のページを既定のブラウザで開く道は作っていない（拒むだけ）。エディタを開いて確かめてはいない。

### D7. ストリームの書き込み口が 1 スレッド専用なのに型で守られていない（修正済み: 第 2 回 A6）

- 場所: [stream.rs:70](../crates/ferritron-core/src/stream.rs)（`unsafe impl Sync for StreamWriter`）、93 行（`write(&self)`）、215 行（`TapWriter::write(&self)`）。
- 内容: `Params` は `Arc` でエディタやワーカーにも渡る（`InstanceHandle::params::<P>()`）。そこから `params.streams().x.write(..)` を呼ぶのは
  `unsafe` なしで書ける。オーディオスレッドと同時に呼ぶと、三重バッファの書き込み側のスロットを 2 スレッドが書く。
- 直し方の案: 書き込み口を `Context` からだけ取れるようにする（`ctx.streams()` が返す型だけに `write` を持たせる）。

**現在の状態**: 第 2 回 A6 で直した。`StreamWriter` と `TapWriter` は `busy` で 1 つずつ書き、重なった書き込みは捨てる（`stream.rs`）。確かめたこと: `two_stream_writers_at_once_never_tear_the_values` と `two_tap_writers_at_once_never_corrupt_the_ring`（それぞれ 1 回ずつ）。残っていること: 捨てた書き込みは `dropped()` に数えない。

### D8. 設定ファイルが壊れていると全部消える（修正済み: T22）

- 場所: [settings.rs:224](../crates/ferritron-core/src/settings.rs)（`read` は失敗を空の表にする）、275 行（`write_key`）。
- 内容: ファイルが JSON として読めないとき（途中で切れた、他のツールが書いた）、`read` は空を返す。次の `set` は「空 + その 1 キー」を書き戻す。
  残りの設定は戻らない。控えも取らない。
- 関連:
  - `get` と `keys` は呼ぶたびに `fs::metadata` を引く（117・128 行）。エディタが毎フレーム読むと、毎フレームのファイル操作になる。
  - `set` はロックファイルを最大 2 秒、名前の付け替えを最大 1 秒待つ。UI スレッドで呼ぶと、その間止まる。
  - 古いロックファイル（5 秒）を消す処理は、2 つのプロセスが同時に「古い」と見ると、片方が作り直したばかりのロックをもう片方が消せる。

**現在の状態**: 第 1 回の T22 で直した。読めない設定ファイルは、書き換える前に `*.json.damaged-<時刻>` として残す（`settings.rs` の `write_key`）。確かめたこと: `a_damaged_file_is_kept_before_it_is_written_again`。残っていること: `get` のたびに `refresh` が `fs::metadata` を引くこと、ロックの待ち（最大 2 秒）、古いロックの消し方は直していない。

### D9. LV2: プリセットとコントロールポート（文書に記載: 第 2 回 C5。一部修正: f4、残り: ホストの画面のつまみは古い値を表示しうる（文書に記載））

- 場所: [ferritron-lv2 lib.rs:278](../crates/ferritron-lv2/src/lib.rs)（`control_events`）、274 行（`activate` で `last` を NaN に戻す）。
- 内容: LV2 のコントロールポートの値はホストのもの。`load_values`（ファクトリプリセット、ユーザープリセット、`presets::load`）で
  プラグインの中の値を変えても、ポートは古い値のまま。ホストの画面も古い値。状態（チャンク）には新しい値が入るが、開き直すとホストがポートの値を
  渡し、`activate` 後の最初のブロックで全ポートがイベントになって、チャンクの値を上書きする。
- 影響: LV2 では、プラグイン側から選んだプリセットが保存後に残らない。形式の制約だが、文書にない。
- 直し方の案: 文書に書く。または、パラメータを `patch:writable` のプロパティとして出す道を足す。

**現在の状態**: f4 の 1-2 で、プリセット（`load_values`、`presets::load`）の値を保存後も残すようにした（`ferritron-lv2/src/lib.rs` の `control_events`）。README（日本語・英語）の LV2 の節には、ポートの値はホストのもので、ホストの画面は利用者が動かすまで古いことがある、と書いた（f7 の 3-2）。確かめたこと: LV2 のホストでは確かめていない（テストは無い）。残っていること: ホストのつまみが古い値を表示しうる（文書に記載）。

### D10. スタンドアロン（文書に記載: 第 2 回 C5）

- 場所: [engine.rs:72](../crates/ferritron-standalone/src/engine.rs)（`midi`）、196 行（`render_block`）。
- ノートオン・オフ以外の MIDI は `_ => return` で捨てる（コントローラ、ピッチベンド、プレッシャー）。`midi_input = true` のエフェクトや
  MPE を使うシンセを試せない。ノートの offset は常に 0。
- トランスポートを渡さない（`ctx.transport()` は常に空）。サイドチェーンなし、2 チャンネル固定。
- 入力は 1 サンプルずつ原子的なリングから取り出す（204 行）。入力と出力のデバイスのクロックがずれると、足りない分は 0 で埋まる（プチノイズ）。

**現在の状態**: 文書に記載した（第 2 回 C5。README は f7 の 3-2 で書き直した）。スタンドアロンの節に、入力の MIDI はノートオンとオフだけ、トランスポートとサイドチェーンは渡さない、入出力は 2 チャンネル固定、と書いた。出力の MIDI は f4 の 2-1 で送るようになったが、入力の機能の追加は、この回の範囲外のまま。

### D11. MCP のトークンの渡し方（修正済み: 第 2 回 B2）

- 場所: [mcp.rs:254](../crates/ferritron-core/src/mcp.rs)（`connect`）。
- 内容: プラグイン側は `127.0.0.1` の決まったポートへつなぎ、最初の行でトークンを送る。相手がハブかどうかは確かめない。
  ハブが動いていないとき、同じマシンの別のプログラムがそのポートで待てばトークンを受け取れ、以後ハブを名乗って AI Access を入れた
  インスタンスを操作できる（パラメータの変更、音の WAV への書き出し）。
- 影響: 同じマシンで動くプログラムに限る。トークンのファイルは既定の権限で書く（Unix では他のユーザーも読める）。
- 直し方の案: ハブが先にトークンから作った応答を示す（チャレンジと応答）。ファイルは本人だけが読める権限で作る。

**現在の状態**: 第 2 回 B2 で直した。トークンそのものは送らず、HMAC-SHA256 の証明を送り合う（`mcp.rs` の `proof`・`verify`、ハブの `challenge`、`tools/ferritron-mcp/src/lib.rs`）。トークンのファイルは Unix では所有者だけが読めるようにした（`write_private`）。確かめたこと: `proof_is_stable_and_depends_on_role_and_inputs`、`verify_accepts_only_the_right_proof`、`a_plugin_proves_the_token_to_the_hub`、`ferritron-mcp` の `a_hub_drops_a_client_that_cannot_prove_the_token`。残っていること: Unix のファイル権限は Unix で確かめていない（Windows でだけ確かめた）。

---

## 性能

| # | 内容 | 場所 |
|---|---|---|
| Q1 | `ctx.value_for` は呼ぶたびに全パラメータをポインタ比較で線形に探す。声 × パラメータ × 区間の回数だけ走る（`Seen::index` の二分探索が使える） | [voice.rs:329](../crates/ferritron-core/src/voice.rs) | （修正済み: T10）
| Q2 | WebAssembly: 音声の受け渡しが 1 サンプルずつ（境界検査つきの `copy_from_slice` を 4 バイトごと）。行きと帰りの両方。まとめて写せる | [ferritron-wasm lib.rs:681](../crates/ferritron-wasm/src/lib.rs)・706 |
| Q3 | Web エディタ: 33 ms ごとに全パラメータの `get_param`（検索と正規化）を呼ぶ。`params_revision` を先に見れば、動いていないときは 0 回にできる | [imp.rs:182](../crates/ferritron-web/src/imp.rs) |
| Q4 | Web エディタ: ストリームは base64 のスクリプトにして `evaluate_script` で送る。2048 個で約 11 KB の文字列を毎回組み立てて評価する。`len` の上限（約 400 万個）では 1 回 22 MB | [imp.rs:201](../crates/ferritron-web/src/imp.rs)、[lib.rs:283](../crates/ferritron-web/src/lib.rs) |
| Q5 | MCP: AI Access が入っている間、オーディオスレッドが処理の前後で全サンプルを 1 個ずつ `%` つきでリングへ写す | [mcp.rs:76](../crates/ferritron-core/src/mcp.rs) |
| Q6 | データストリームの読み取りは `Mutex` の中で全体を写し、`latest()` は毎回 `Vec` を作る | [stream.rs:153](../crates/ferritron-core/src/stream.rs)・166 |
| Q7 | スタンドアロン: 入力を 1 フレームずつ原子操作 2 回と `%` で出し入れする | [engine.rs:244](../crates/ferritron-standalone/src/engine.rs) |
| Q8 | LV2: `run` のたびに拡張を名前で引く（`get_extension` の文字列比較が 3 回）。`instantiate` で取っておける | [ferritron-lv2 lib.rs:436](../crates/ferritron-lv2/src/lib.rs)・440・485 |

**現在の状態**（性能の Q。この回の範囲外。`docs/performance.md` の方針どおり測ってから直す）:

- **Q1** 修正済み（第 1 回の T10）。
- **Q2** 未修正。
- **Q3** 未修正（第 1 回の T31 として飛ばした。任意項目）。
- **Q4** 未修正。
- **Q5** 未修正。
- **Q6** 未修正。
- **Q7** 未修正。
- **Q8** 未修正。

---

## 小さな問題

- **E1** LV2・VST3・CLAP とも、入力を出力へ写すのはチャンネルの順。ホストが「入力 1 と出力 0 が同じバッファ」のように互い違いに渡すと、
  写す前に入力を壊す（LV2 は `inPlaceBroken` を名乗らない限り許される）。

  **現在の状態**: 未修正。入力と出力のチャンネルの順は変えていない（この回の範囲外）。
- **E2** Web エディタ: ファイルのダイアログ（`dialog:`）はメッセージの処理の中でモーダルに開く。その間もタイマーの `refresh` が同じ `View` を
  `&mut` で触る（[imp.rs:243](../crates/ferritron-web/src/imp.rs)・386）。 （修正済み: 第 2 回 C2）

  **現在の状態**: 第 2 回 C2 で直した。ダイアログを開いている間は `in_dialog` が真になり、タイマーの `refresh` は何もしない（`ferritron-web/src/imp.rs`）。確かめたこと: `cargo build -p gain`（web 機能つき）と `cargo test -p ferritron-web`（12 件）。ダイアログを実際に開いては確かめていない。
- **E3** `dsp::Smoother` の `Exp` は `set_time_constant` を呼ばないと係数が 1.0 で、1 サンプルで目標へ飛ぶ。`samples` が時定数より短いと、
  最後に目標へ飛ぶ（段差）。どちらも文書どおりだが、既定で段差になる側に倒れている（[smoother.rs](../crates/ferritron-dsp/src/smoother.rs)）。

  **現在の状態**: 未修正（`dsp::Smoother` は変えていない）。
- **E4** `Lookahead::set_lookahead` は検出器の位置をその場で変える。音声の途中で呼ぶと検出値が飛ぶ（文書どおり。補間する版がない）。

  **現在の状態**: 未修正（`Lookahead` は変えていない）。
- **E5** データストリームはエディタが開いていないと書き込みを捨てる。ワーカーや `Harness` から読むときも同じ（`set_editor_open` が要る）。

  **現在の状態**: 未修正。書き込みを捨てる仕組みは変えていない。
- **E6** LV2: イベントは 1 ブロック `MAX_EVENTS` 個まで。超えた分は何も言わずに捨てる（コントロールポートの変更も）。

  **現在の状態**: 未修正。LV2 は `MAX_EVENTS` を超えた分を今も捨てる（`ferritron-lv2/src/lib.rs`）。
- **E7** WebAssembly: 状態は `SCRATCH` より大きいと保存も読み込みもできず、`save_state` は大きさだけ返して中身を写さない（前の文書の B7 と同じ形）。（修正済み: 第 2 回 B5。大きいときは 0 を返し、シムは空の状態を保存する）

  **現在の状態**: 第 2 回 B5 で直した。`save_state` は、状態が `SCRATCH` より大きいと 1 行を `eprintln!` で出して 0 を返す（`ferritron-wasm/src/lib.rs`）。シムは 0 を失敗とは扱わず、空の状態を保存する（VST3 と CLAP のコードを読んで確かめた）。確かめたこと: `cargo build -p ferritron-wasm`。大きな状態を実際に作る試験は無い。
- **E8** MCP のエージェントは、AI Access が入っている間 200 ms ごとに全エンドポイントの一覧を文字列にして比べる。

  **現在の状態**: 未修正（この回では扱っていない）。
- **E9** CI は `.github/workflows/macos.yml` だけ。その中の Windows のジョブは macOS 向けのバンドルを作るためのもので、テストは macOS のジョブで
  一部のクレートだけ回す。Windows（検証済みとしている唯一の環境）で `cargo test --workspace` と検査用ホストを回すジョブがない。

  **現在の状態**: 未修正。`.github` は変えていない。

---

## 前の文書への追記

- **B2（バイパス中のノートオフ）: 実行で確認。** ノートオン → バイパス → ノートオフ → バイパス解除の順に流すと、プラグインが見たノートは
  `On key=60` だけだった。
- **B11（状態にないパラメータ）: 実行で確認。** `extra` を 3.5 にしたインスタンスに、`extra` を含まない状態を読ませると、
  読み込みは成功し、`extra` は 3.5 のまま（既定値は 1）。
