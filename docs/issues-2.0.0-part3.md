# Ferritron 2.0.0 の問題点の洗い出し（その 3）

2026-11-10、ブランチ `ferritron-2.0`（`c72816c`）。[issues-2.0.0.md](issues-2.0.0.md)、[issues-2.0.0-part2.md](issues-2.0.0-part2.md) の続きで、
ホットリロード、GPU エディタ、GPU 処理、Ferritron Native Graphics（FNG）を調べた記録です。番号は H（ホットリロード）、V（GPU エディタ）、
G（GPU 処理）、N（FNG）。修正はしていません。これらから出た機能の要望は [feature-requests.md](feature-requests.md) の F26〜F45 に足しました。
FNG の CPU レンダラ（`cpu.rs`）は後から調べ、末尾の「追記」（U1〜U10、要望 F46〜F51）にまとめました。

## 調べた範囲と方法

- **読んだもの**
  - ホットリロード: `ferritron-core/src/reload/proxy.rs`（全体）。
  - GPU エディタ: `ferritron-gpu-view` の `imp.rs`（Windows の実装、全体）、`common.rs`（全体）、`lib.rs` と `pacing.rs` は関数の一覧まで。
  - GPU 処理: `ferritron-gpu` の `lib.rs`（`Convolver` と GPU スレッド）、`batch.rs`（全体）、`kernel.rs`（`GpuKernel` と `KernelNode`）、
    `ferritron-graph` の `process` と `choose`。
  - FNG: `atlas.rs`、`prepare.rs` のマスクのキーとやり直しの部分、`gpu.rs` のテクスチャとアップロードの部分。
- **読んでいないもの**: `ferritron-gpu` の `iir.rs`・`room.rs`・`nodes.rs` の中身、`ferritron-vulkan`、`ferritron-slang`、
  GPU エディタの macOS 実装（`mac.rs`）、FNG の `cpu.rs`・`text.rs`・`stroke.rs`・`raster.rs`・`font.rs`。**これらに問題がないという意味ではありません。**
- **この文書の項目はすべてコードを読んで確かめただけで、実行していません。** GPU の動作、ウィンドウの動作、ホットリロードは今回動かしていません。
  性能の項目は測っていません。

## 一覧

| # | 重要度 | 内容 |
|---|---|---|
| H1 | 高 | ホットリロードのプロキシが、レイテンシ・オフライン・エディタ以外の拡張を返さない（開発用バンドルではトランスポートもサイドチェーンも無い） |
| H2 | 中 | 古いライブラリをアンロードするとき、そのライブラリが始めたスレッドが残っていると落ちうる |
| H3 | 中 | `wait_for_readers` は全インスタンス共通の計数が 0 になるまで回る。処理が重なっているとリロードも `destroy` も終わらない |
| H4 | 低 | VST2 シムが持つパラメータ名のポインタが、2 回リロードすると解放済みになる |
| V1 | 高 | wgpu のエラーとデバイスの喪失を受けていない。エディタの外で起きるとホストごと落ちる |
| V2 | 高 | ウィンドウクラスを登録解除しない。DLL を同じ番地に読み直すと、以後エディタが開かない |（修正済み: T28）
| V3 | 中 | 入力イベントのたびに描く（ペーサーを通らない）。ドラッグ中は表示の周期より速く描きうる |
| V4 | 中 | IME で確定した文字が届かない可能性が高い（日本語入力） |
| V5 | 中 | マウスのキャプチャを失ったときの処理がない。ドラッグが終わらず、以後のキャプチャも取らなくなる |
| V6 | 低 | フレームを待つスレッドが、用がなくても 3 ms ごとに起きる |
| G1 | 高 | `Convolver`: GPU が遅れると `broken` が立ち、作り直すまで残響のテールが戻らない |
| G2 | 中 | `Convolver`: 結果があっても `try_lock` が外れると、そのグリッド 1 つ分のテールが抜ける |（修正済み: T27）
| G3 | 中 | GPU のデバイスが応答しなくなると、GPU スレッドが止まり、`Convolver` の解放（メインスレッド）も戻らない |
| G4 | 中 | `choose` はアクティベートのたびに測って CPU / GPU を決める。レイテンシが回によって変わりうる |
| N1 | 高 | アトラスに追い出しがない。いっぱいになると全部消してそのフレームをやり直す |
| N2 | 高 | パスのマスクのキーに、平行移動を含む変換と画面の大きさが入る。動く図形とリサイズで毎フレーム新しいマスクになる |
| N3 | 低 | テクスチャの大きさをデバイスの上限と比べていない |
| R1〜R6 | 性能 | 下の「性能」の章 |
| U1〜U10 | 中〜低 | FNG の CPU レンダラ（`cpu.rs`）。下の「追記」の章。速さは実測 |

---

## H. ホットリロード

### H1. プロキシが返す拡張が 3 つだけ

- 場所: [proxy.rs:370](../crates/ferritron-core/src/reload/proxy.rs)（`get_extension`）。`EXT_LATENCY`・`EXT_RENDER`・`EXT_VIEW` 以外は `null`。
- 内容: 開発用バンドル（`--dev`）では、シムから見て次の拡張が無い: トランスポート、サイドチェーン、レイアウト、ホストとの連絡（v1 / v2）、
  トラック、メインスレッドの依頼、スリープ、エディタの大きさ、声の数、記述（describe）、ARA。
- 影響（開発中だけ。リリースのバンドルには関係しない）:
  - `ctx.transport()` は常に空、`ctx.sidechain()` は常に `None`、スピーカーの並びは既定のまま。
  - エディタが自分のインスタンスを見つけられない（`for_shim_ctx` が当たらない）ので、`[streams]`、`on_message` / `post`、`store` が動かない。
  - `request_latency`・`load_values`・`notify` がホストへ届かない（`notify` は 0.1 秒後に別スレッドで呼ばれる）。
  - 無音のフラグ（`silent`）が出ないので、ホストから見たスリープの振る舞いが変わる。
- 結果として、開発中に動かして確かめたものと、リリースで動くものが違う。「開発用バンドルで試したらテンポが取れない」は、プラグインの不具合に見える。
- 直し方の案: 各拡張に、プロキシのポインタを今のスロットの `core` に替えて転送する関数を置く（`proxy_latency` と同じ形）。

### H2. 古いライブラリのアンロード

- 場所: [proxy.rs:280](../crates/ferritron-core/src/reload/proxy.rs)〜283（`retired` と `expired` を落とす）。
- 内容: 読み込んだ写しはそれぞれ自分の静的変数とスレッドを持つ。インスタンスが使うワーカーは最後のインスタンスで join するが、
  `ferritron-notify` と `ferritron-watch`（[issues-2.0.0.md](issues-2.0.0.md) の B3）は止まらない。2 世代前の写しを `FreeLibrary` した時点で、
  そのスレッドの戻り先が無くなる。`on_notify` か `instances_watch` を使うプラグインで当たる。
- 一時フォルダの写し（`<名前>-<pid>-<世代>.dll`）は `Drop` で消す。ホストが落ちると残り、次に消すものがない。

### H3. `wait_for_readers`

- 場所: [proxy.rs:114](../crates/ferritron-core/src/reload/proxy.rs)。`READERS` は全インスタンス・全スレッド共通の 1 つの計数。
- 内容: リロードと `destroy` は、状態のロックを持ったまま、この計数が 0 になる瞬間を `yield_now` で待つ。ホストが複数のオーディオスレッドで
  多くのインスタンスを途切れなく処理していると、0 になる瞬間が来にくい。その間、メインスレッドの `destroy`・`activate`・状態の保存も
  同じロックで止まる。
- 直し方の案: インスタンスごとの世代番号にする（スロットを替えた後、そのインスタンスの読み手だけを待つ）。

### H4. その他

- VST2 シムは作成時に `ferritron_param_info`（名前のポインタを含む）を配列に取っておく。プロキシは 1 世代前までしか残さないので、
  2 回リロードすると名前が解放済みのメモリを指す（[vst2_shim.cpp:46](../crates/ferritron-vst2/shim/vst2_shim.cpp)、[proxy.rs:74](../crates/ferritron-core/src/reload/proxy.rs)）。
- 読み込みが一時的な失敗（`Transient`）を返し続けると、250 ms ごとに DLL 全体を一時フォルダへ写してはやめ、記録に 1 行ずつ足す。
- `PROXY_VIEW` は最初のビューの大きさを覚えたまま。`[view] size` を変えてリロードしても、ホストへ言う大きさは変わらない。
- 移行（`migrate`）は、古いインスタンスが処理を続けている間に状態を写す。写した後、差し替えるまでに届いたパラメータの変更は落ちる。

---

## V. GPU エディタ

### V1. wgpu のエラーとデバイスの喪失（一部修正: f5・f6・f7。f7 の共有デバイスの作り直しは手で立てた印で模擬。残り: 実機のデバイス喪失は未確認。GPU が無いときの CPU 描画は FNG だけ）

- 場所: `ferritron-gpu-view`・`ferritron-gpu`・`ferritron-native-graphics`・`ferritron-graph`・`ferritron-vulkan` のどこにも、
  `on_uncaptured_error`、エラーのスコープ、デバイス喪失のコールバックが出てこない。
- 内容: wgpu は、受け手のないエラーで panic する。エディタの `render` / `input` の中なら `Canvas::editor` の `catch_unwind` が受けるが、
  その外にも wgpu の呼び出しがある: `surface.configure`（[common.rs:172](../crates/ferritron-gpu-view/src/common.rs)、失われたサーフェスの作り直し）、
  `get_current_texture`、`queue.present`。これらは `window_proc`（`extern "system"`）から直接呼ばれる（`WM_PAINT`、タイマー）。
  panic が `extern` を越えると Rust はプロセスを止める。
- 起きる場面: GPU ドライバの再起動や更新、リモートデスクトップへの切り替え、外付け GPU の取り外し、ノート PC の GPU の切り替え。
- 共有のデバイスは `OnceLock` に 1 つ（[common.rs:29](../crates/ferritron-gpu-view/src/common.rs)）。失われた後に作り直す道がないので、
  落ちなかったとしても、そのプロセスでは以後どのエディタも描けない。
- GPU が 1 つも使えない環境（一部の仮想マシン、リモート）では `shared()` が失敗し、エディタは開かない。CPU での描画に切り替える道がない
  （FNG には `CpuRenderer` がある）。

**現在の状態**: 一部直した（f5・f6・f7）。ウィンドウ手続き（`window_proc`）は `catch_unwind` で包み、panic したビューは以後描かない（`mark_broken`）。`on_uncaptured_error` と `set_device_lost_callback` で「失われた」印を立て、次の描画の前に `renew` で共有デバイスを作り直す（世代番号つき、`common.rs`）。作り直しが 3 回続けて失敗すると、`GpuEditor::new_cpu` と `render_cpu` を持つ FNG（`FngAdapter`）だけ CPU で描き、5 秒ごとに GPU を試す（Windows のみ。`FERRITRON_GPU_VIEW=cpu` で最初から CPU）。確かめたこと: `common::tests::a_lost_device_is_made_again_with_the_next_generation`、`tests/cpu_view.rs`（CPU で描いた画素）。残り: 実機のデバイス喪失とドライバの再起動は起こしていないので、作り直しと CPU への切り替えは実際の喪失では未確認。ほかの GPU エディタは、GPU が無いと開かない。

### V2. ウィンドウクラス

- 場所: [imp.rs:607](../crates/ferritron-gpu-view/src/imp.rs)〜629。
- 内容: クラス名はウィンドウプロシージャの番地から作り、`RegisterClassExW` が 0 を返すと `None` にする。`UnregisterClass` はどこにもない。
  クラスはホストの EXE のモジュール（`GetModuleHandleW(null)`）の下に登録する。
- 影響: ホストがプラグインの DLL をアンロードして読み直し、同じ番地に載ると（よくある）、同じ名前のクラスが残っているので登録が失敗し、
  `create` が `None` を返す。**そのプロセスでは以後エディタが開かない**。別の番地に載ると、古いクラスは解放済みのプロシージャを指したまま残る。
- 直し方の案: `ERROR_CLASS_ALREADY_EXISTS` を成功として扱う（同じ番地なら同じプロシージャ）。最後のビューを閉じるときに登録を解除する。

**現在の状態**: 一部直した（T28）。`RegisterClassExW` が `ERROR_CLASS_ALREADY_EXISTS` を返したときは成功として扱う（`imp.rs`）。クラス名はウィンドウ手続きの番地から作るので、同じ番地に読み直した DLL は同じクラスを使い、以後もエディタが開く。`UnregisterClass` はまだ無く、最後のビューを閉じても登録は残る。別の番地に読み直したときは、古いクラスが解放済みの手続きを指したまま残る（確かめていない）。

### V3. 入力のたびに描く（修正済み: f5）

- 場所: [common.rs:241](../crates/ferritron-gpu-view/src/common.rs)〜248（`input` が真を返すと、その場で `render`）。
- 内容: フレームの間隔を決める `Pacer` は `tick` からの描画にだけ効く。マウスの移動は 1 秒に数百〜1000 回届くことがあり、
  エディタが「描き直す」を返すたびに 1 フレーム描く。表示のモードは待たないもの（Mailbox / Immediate）を選ぶので、止めるものがない。
- 影響: つまみのドラッグ中に、GPU とメインスレッドを表示の周期の何倍も使う。重いエディタではホストの UI が遅くなる。
- 直し方の案: `input` は `dirty` を立てるだけにして、描くのは `tick`（ペーサー）に任せる。

**現在の状態**: 直した（f5）。`input` は描かず `dirty` を立てるだけにして、描くのは次の確認（`tick`）に任せる（`common.rs` の `input`）。入力のあとはティッカーを起こすので、遅れは 1 フレーム以内。確かめたこと: `tests/window.rs` の `input_that_asks_for_a_frame_is_drawn_by_the_next_check_not_inside_the_message`（50 回の入力の間に描画が 0 回）。速さは測っていない。

### V4. IME（修正済み: f5。実際の IME では未確認）

- 場所: [imp.rs:470](../crates/ferritron-gpu-view/src/imp.rs)〜510。
- 内容: 文字（`WM_CHAR`）をエディタへ渡すのは、直前のキー押下をエディタが `wants_key` で欲しいと言ったときだけ（`TAKES_TEXT`）。
  IME の変換中、キー押下は `VK_PROCESSKEY`（0xE5）で届き、`Key::Other(0xE5)` になる。エディタがこれを欲しいと言わなければ、
  確定した文字は捨てられ、キーは親ウィンドウへ送られる。`WM_IME_*` の処理も、変換中の文字列の位置を伝える処理もない。
- 影響: GPU エディタの文字入力欄（プリセット名など）で、日本語などの IME を使う入力ができない可能性が高い。実行では確かめていない。

**現在の状態**: 直した（f5）。エディタが `Ui::set_text_input` で文字入力中と入力欄の位置を知らせると、`VK_PROCESSKEY` のキーをエディタへ渡し、確定した文字は `WM_IME_COMPOSITION` から `ImmGetCompositionStringW` で取り出す（`imp.rs`）。変換窓は入力欄の位置に出る。確かめたこと: コードを読んだだけ。実際の IME では確かめていない（テストも書けなかった）。

### V5. キャプチャを失ったとき（修正済み: f5）

- 場所: [imp.rs:511](../crates/ferritron-gpu-view/src/imp.rs)〜534。`WM_CAPTURECHANGED`・`WM_CANCELMODE` を扱っていない。
- 内容: ボタンを押すと `held` を増やして `SetCapture`、離すと減らす。ドラッグ中に Alt+Tab、モーダルなダイアログ、ホストの操作で
  キャプチャを失うと、離した知らせが届かない。`held` は 1 のまま残り、以後は `held == 0` のときだけ呼ぶ `SetCapture` を呼ばず、
  `Leave` も送らない。エディタは「押したまま」の状態で止まる。
- ポインタのロック（`ClipCursor`）も同じ。ウィンドウが非アクティブになると Windows はクリップを外すが、`lock` は残る。

**現在の状態**: 直した（f5）。`WM_CAPTURECHANGED` と `WM_CANCELMODE` で `capture_lost`（`imp.rs`）が、押していたボタンの `Release` と `Leave` を送り、ポインタのロック（`ClipCursor`）を外す。非アクティブになったとき（`WM_KILLFOCUS`）も外す。確かめたこと: `tests/window.rs` の `losing_the_capture_while_a_button_is_held_sends_a_release_and_a_leave`（`ReleaseCapture` で捕獲を奪う）。ロックの解除は読んだだけ。

### V6. その他（一部修正: f5、残り: モニターごとの周期は未対応。Fifo だけの環境では描画の頻度を半分にしただけ）

- フレームを待つスレッド（`Ticker`）は、期限がないとき 3 ms ごとに起きる（[imp.rs:141](../crates/ferritron-gpu-view/src/imp.rs)）。
  一度始まると、エディタを閉じるまで 1 秒に約 330 回。`T9`（静止中は描画 0 回）は描画の回数で、この起床は数えていない。
- 表示の周期は `DwmGetCompositionTimingInfo` の 1 つの値で、モニターごとではない。60 Hz と 144 Hz を並べていると、片方に合わない。
- 待たない表示モードが無い環境（`Fifo` だけ）では、`present` がホストの UI スレッドを垂直同期まで止める。エディタの数だけ重なる。

**現在の状態**: 一部直した（f5）。フレームを待つ `Ticker` は、期限が無いときは `Condvar` で眠り、3 ms ごとには起きない（起床の回数は測っていない）。表示の周期は、窓のあるモニターの値を `EnumDisplaySettingsW` で読み、`WM_MOVE` と `WM_DISPLAYCHANGE` のあとと、120 回の確認ごとに読み直す（`common.rs`、`imp.rs`）。`Fifo` だけの環境では描画の頻度を半分にするが、`present` は UI スレッドのままなので、待ちは残る。残り: モニターごとの周期を持つ仕組みはない。

---

## G. GPU 処理

### G1. `broken` は戻らない（一部修正: f5。再開は `restarts()` で数える。`ctx.notify` での通知は未実施）

- 場所: [ferritron-gpu lib.rs:324](../crates/ferritron-gpu/src/lib.rs)〜328、405〜408、348。
- 内容: GPU スレッドへ渡す前の入力は `pending`（容量 `max_block × 16` サンプル）にためる。あふれると `broken` を立て、以後 `fetch_tail` は
  何もせず戻る。`broken` を下ろす箇所はない。テールは `Convolver` を作り直す（再アクティベート）まで出ない。
- 影響: ブロックが 64 サンプルなら、容量は 1024 サンプル = 48 kHz で約 21 ms。GPU がそれだけ止まる（他のアプリの描画、ドライバ、
  省電力からの復帰）と、**残響の尾が無言で消え、戻らない**。分かるのは `missed_blocks()` が増え続けることだけ。
- 直し方の案: 遅れたら古い入力を捨てて、今の位置から畳み込みを始め直す（数百 ms の空白の後に戻る）。`ctx.notify` でエディタへ知らせる。

**現在の状態**: 直した（f5）。`broken` に当たる旗は、今の `Convolver` には無い。GPU へ送る入力があふれたら（`send_input`）、溜めた入力を捨て、今の位置から窓を数え直して入力の流れを始め直す。テールは数ミリ秒の空白の後に戻る。再開の回数は `Convolver::restarts()` で数える。確かめたこと: `a_stalled_gpu_restarts_its_stream_and_the_tail_comes_back`（GPU を止めて、あふれ、戻ることを確かめた）。残り: `ctx.notify` でエディタへ知らせることはしていない（`restarts()` を見てプラグインが知らせる）。実際の GPU の停止では確かめていない。

### G2. `try_lock` が外れるとグリッド 1 つ分が抜ける

- 場所: [ferritron-gpu lib.rs:345](../crates/ferritron-gpu/src/lib.rs)〜372。
- 内容: `fetch_tail` は最初に `tail_block = Some(g)` と置いてから `try_lock` する。GPU スレッドが同じロックを持っている瞬間
  （入力を取り出す、結果を置く）に当たると、結果ができていても取れず、`tail_valid = false` のままそのグリッド（64〜1024 サンプル）を終える。
  同じグリッドの中でやり直さない。
- 影響: 確率は低いが、長く回せば起きる。起きると、テールが 1 グリッド分だけ無音になる（プチッという切れ目）。
- 直し方の案: 外れたら `tail_block` を置かず、次のサンプルでもう一度試す。または結果の受け渡しを待たない仕組み（`util::Swap`）にする。

**現在の状態**: 直した（T27）。`fetch_tail` は、`try_lock` が外れたときは `tail_block` を置かずに戻るので、次のサンプルで同じグリッドを取り直す（`ferritron-gpu` の `lib.rs`）。確かめたこと: コードを読んだだけ（この項目のテストは無い）。

### G3. 応答しない GPU（修正済み: f5。コードを読んで確かめただけ）

- 場所: [ferritron-gpu lib.rs:483](../crates/ferritron-gpu/src/lib.rs)（`device.poll(wait_indefinitely())`）、[batch.rs:96](../crates/ferritron-gpu/src/batch.rs)〜100（`join`）。
- 内容: GPU スレッドは結果を期限なしで待つ。デバイスが応答しなくなると戻らない。最後の `Convolver` を落とすとスレッドを `join` するので、
  プラグインを消す操作（ホストのメインスレッド）も戻らない。オフラインのモードでは、オーディオスレッドが `done.wait_while` で同じく待ち続ける。
- GPU スレッドが panic で終わると（V1 と同じ理由）、`Registry` は終わったスレッドを持ったまま。後から作った `Convolver` も同じ所へ登録され、
  結果は来ない。

**現在の状態**: 直した（f5）。GPU の 1 回の応答は 2 秒（`GPU_WAIT`）まで待ち、過ぎたら GPU は失われたものとして、入力をそのまま出す（実時間の処理）。オフラインでも同じ期限で待つ。GPU の呼び出しが panic したときも失われた扱いにする（`ferritron-gpu` の `lib.rs`）。最後の `Convolver` を外すとき、`Registry::unregister` は GPU スレッドの終わりを 2 秒（`EXIT_WAIT`）まで待ち、来なければ切り離す（`batch.rs`）。GPU スレッドが panic で終わったら、次の登録で新しいスレッドを作る。Vulkan の fence 待ちも 2 秒（`FENCE_WAIT`）。確かめたこと: 応答しない GPU は作れなかったので、期限の経路はコードを読んだだけ（期限を実際に過ぎたことはない）。

### G4. 置き場所を測って決める（一部修正: f5・f6・f7。f7 で GPU を失ったときの CPU への切り替えが入った（レイテンシの変化は `take_latency_changed()` で知らせる）。残り: `KernelNode` は今も毎回測り、鍵も固定もない）

- 場所: [ferritron-graph lib.rs:507](../crates/ferritron-graph/src/lib.rs)〜552（`choose`）。
- 内容: 候補（CPU、GPU）をアクティベートのときに無音で 10 回走らせ、速いほうを選ぶ。`Policy::Max` では、レイテンシのある GPU の候補は
  「CPU の最良がブロックの時間の 1/8 を超える」ときに選ばれる。測定はそのときのマシンの負荷に左右される。
- 影響: 同じプロジェクトでも、開くたびに、あるいはインスタンスごとに、CPU と GPU が入れ替わりうる。入れ替わるとレイテンシが変わり、
  ホストの遅延補正と書き出しの結果が変わる。オフラインでは GPU が待つ（レイテンシなし）ので、再生と書き出しでも経路が違う。
- 直し方の案: 選んだ結果を状態に保存し、次からはそれを使う（利用者が明示的に測り直す）。

**現在の状態**: 一部直した（f5・f6・f7）。`IirNode` と `MatrixMixerNode` は `choose_keyed` を使い、構成ごとに覚えた置き場所を使う（`ferritron-gpu` の `nodes.rs`）。`Graph::placement` と `Graph::set_placement` で、プラグインが置き場所を決められる。`IirNode`・`KernelNode`・`MatrixMixerNode` は、GPU を失ったとき CPU の候補へ切り替える（`lost()` と `fall_back()`。手で立てた印で試し、`KernelNode` は試していない）。切り替えで遅延が変わったときは `Graph::take_latency_changed()` が一度だけ真になる。確かめたこと: `placement_is_remembered_and_can_be_fixed`、`set_placement_rebuilds_the_node_and_prepares_again`。残り: `KernelNode` は今も `choose` を毎回呼び、鍵も固定もない（`kernel.rs`）。`IirNode` の `Bank::Gpu` は作り直さない。`ConvolverNode` には CPU の候補が無いので切り替えない。

### G5. その他（一部修正: f5・f7。残り: 1024 サンプルのブロックで GPU が間に合わず欠落する。f7 では max_block 256 で 300 回中 5、max_block 1024 で 2 秒の測定に 0 と測った（f6 の欠落 553 は再現せず）。原因は未調査）

- `Convolver::process` と `Graph::process` は、`max_block` を超えるブロックの先頭だけを処理し、残りを入力のまま残す
  （[ferritron-gpu lib.rs:377](../crates/ferritron-gpu/src/lib.rs)、[ferritron-graph lib.rs:402](../crates/ferritron-graph/src/lib.rs)）。文書は「超えてはならない」と言うが、超えたときに乾いた音が混ざる。
- GPU スレッドは、仕事がなくても 1 ms ごとに起きる（[batch.rs:39](../crates/ferritron-gpu/src/batch.rs)）。`Convolver` が 1 つでもあれば、止まっている間も 1 秒に 1000 回。

**現在の状態**: 一部直した（f5・f7）。`Convolver::process` と `Graph::process` は、`max_block` を超えるブロックを `max_block` ごとに区切って全部処理するので、先頭だけを処理して残りを乾いたまま流すことはない（上の本文の 1 つ目は直る前の説明）。GPU スレッドは仕事がなければ眠り、通知で起きる（確認は 10 ms ごと、`batch.rs`）。超えた呼び出しは一度だけ警告を出す（`an_oversized_call_is_processed_whole_and_noted_once`）。f7 の測定: 1024 サンプルの呼び出しを `max_block` 256 で流すと（契約の外）、2 秒の測定で 7 回、300 回（約 6.4 秒）で 5 回欠落した。`max_block` 1024 では 2 秒の測定で欠落 0。256 のときは欠落 0。f6 の欠落 553 回は、この測り方では再現しなかった（原因は未調査）。目標の「256 のときの 2 倍以内」は未達。残り: 1024 サンプルのブロックで GPU が実時間に間に合わないことがあり、欠落は `missed_blocks()` で見える。数字は `docs/performance.md`。

---

## N. Ferritron Native Graphics

### N1. アトラスに追い出しがない（修正済み: f5）

- 場所: [atlas.rs:97](../crates/ferritron-native-graphics/src/atlas.rs)〜130、[prepare.rs:312](../crates/ferritron-native-graphics/src/prepare.rs)〜322、442〜453。
- 内容: 各エントリは最後に使ったフレーム（`used`）を記録するが、読む箇所がない。アトラス（最大 4096 × 4096）がいっぱいになると、
  アトラス・グラデーションの表・レイヤーを**全部消し**、そのフレームを最初から組み直す。2 回目もいっぱいなら、入らないものは描かない（`dropped`）。
- 影響: やり直しのフレームでは、画面にあるすべてのグリフと図形を CPU で塗り直し、アトラス全体（最大 16 MB）をアップロードする。
  その 1 フレームが長くなる。N2 と合わさると、これが周期的に起きる。

**現在の状態**: 一部直した（f5）。いっぱいになると、そのフレームで使っていない最も古い棚を空けて、そこへ入れる（`atlas.rs` の `evict_one_shelf`）。棚がすべてこのフレームで使われているとき、または 4096 px を超えるマスクのときだけ、従来どおり全部を消して 1 回だけ組み直す（`prepare.rs` の `Restart`）。2 回目も入らないものは、そのフレームでは描かない（`dropped`）。1 フレームの新しいマスクがアトラスの 1/4 を超えたあとは、そのフレームだけのマスクになり、キャッシュに入れない。確かめたこと: `atlas.rs` の `a_full_atlas_evicts_the_oldest_shelf_only`、`masks_past_the_frame_budget_are_for_this_frame_only`、`tests/atlas_eviction.rs`（人工の 600 枚で再開始が無いこと）。残り: 実際の絵で追い出しの頻度と描き直しの量は測っていない。

### N2. マスクのキー（修正済み: f5・f6）

- 場所: [prepare.rs:516](../crates/ferritron-native-graphics/src/prepare.rs)（塗り）、526（線）、730（変換つきのグリフ）、766。
- 内容: パスのマスクのキーは「パスの中身のハッシュ + 変換行列の 6 個の値のビット + 描画先の大きさ」。変換には平行移動も入る。
  - 位置だけ違う同じ図形（リストの行、鍵盤、目盛り）は、それぞれ別のマスクになる。
  - 動く図形（つまみの針、メーター、スペクトルの線）は、毎フレーム新しいマスクになる。古いものは N1 のとおり、いっぱいになるまで残る。
  - 描画先の大きさがキーに入るので、ウィンドウのリサイズ中は全部のマスクが毎フレーム新しくなる。
- 影響の見積もり（測っていない）: 800 × 300 ピクセルのスペクトルの線を毎フレーム描くと、1 フレームで約 24 万ピクセル。
  4096² = 約 1680 万ピクセルなので、約 70 フレーム（60 fps で 1 秒強）でいっぱいになり、N1 のやり直しが 1 秒ごとに入る。
- 直し方の案: キーの平行移動は小数部（サブピクセル）だけにし、整数部は描くときのずらしで済ます。毎フレーム変わるパスはアトラスへ入れず、
  そのフレームだけの領域に置く。追い出しは `used` の古い棚から。

**現在の状態**: 直した（f6。f5 は一部で止めた）。マスクのキーは、図形と変換の小数部（1/4 px に丸めたもの）で作り、整数部の平行移動と描画先の大きさは入れない（`prepare.rs` の `Shape`）。整数だけ位置の違う図形と、リサイズのあとでも同じマスクを使う。4000 px を超える図形は、従来のキー（描画先の大きさを含む）のまま。確かめたこと: `tests/mask_keys.rs` の 4 件（`one_mask_serves_every_whole_pixel_position` など）。残り: 動く図形がアトラスをどれだけ使うかは測っていない。描く位置は最大 1/8 px ずれる。

### N3. テクスチャの大きさ（一部修正: f5・f6、残り: グループのテクスチャは親と同じ大きさのまま、上限を別に見ていない）

- 場所: [gpu.rs:357](../crates/ferritron-native-graphics/src/gpu.rs)〜387（グループとレイヤーのテクスチャ）、[common.rs:46](../crates/ferritron-gpu-view/src/common.rs)（サーフェス）。
- 内容: 大きさをデバイスの上限（既定の制限で 8192）と比べていない。上限を超えると wgpu の検証エラーになり、V1 のとおり panic する。
  高い倍率の大きなモニターでリサイズできるエディタを最大にすると届きうる（3840 × 2.5 = 9600）。

**現在の状態**: 一部直した（f5・f6）。レイヤーのテクスチャは、デバイスの `max_texture_dimension_2d` を超えないように縮めて描き、貼るときに引き伸ばす（`Planner::max_dim`。`gpu.rs` で設定）。確かめたこと: `tests/texture_limit.rs` の 2 件。サーフェスの大きさは `ferritron-gpu-view` の `clamp_size` で上限に切る（f5）。残り: グループの一時テクスチャは親と同じ大きさのままで、上限を別に見ていない（描画先そのものが上限を超えるときはサーフェス側の仕事）。

---

## 性能（測っていない）

| # | 内容 | 場所 |
|---|---|---|
| R1 | `Convolver` の頭（CPU の部分）は、マスクつきのリングを 1 タップずつ読む総当たり。タップ数は最大で `max_block` の約 3 倍。512 サンプルなら 1 ブロックあたり約 1536 × 512 ≈ 79 万回の積和、2048 サンプルなら約 1260 万回（チャンネルごと）。「GPU の畳み込み」の CPU 側が支配的になりうる。頭も分割 FFT にできる | [ferritron-gpu lib.rs:383](../crates/ferritron-gpu/src/lib.rs)〜401 |
| R2 | GPU エディタは 16 ms ごとに全パラメータを読み、`Vec<f64>` を作って前回と比べる（開いているエディタごと）。`params_revision` を先に見れば、動いていないときは読まずに済む | [common.rs:198](../crates/ferritron-gpu-view/src/common.rs)、`Ui::snapshot` |
| R3 | フレームを待つスレッドの 3 ms ごとの起床（V6）と、GPU スレッドの 1 ms ごとの起床（G5）。イベントで待てる | [imp.rs:141](../crates/ferritron-gpu-view/src/imp.rs)、[batch.rs:39](../crates/ferritron-gpu/src/batch.rs) |
| R4 | グラフの遅延補正のリングは、1 サンプルごとに `%` を 2 回する | [ferritron-graph lib.rs:426](../crates/ferritron-graph/src/lib.rs)〜430 |
| R5 | FNG: アトラスが育つたび、消えるたびに全体をアップロードする（最大 16 MB）。N1・N2 が直れば回数が減る | [gpu.rs:308](../crates/ferritron-native-graphics/src/gpu.rs) |
| R6 | ホットリロード中は、`process` のたびに全インスタンス共通の計数を `SeqCst` で 2 回書く（スレッド間で同じキャッシュラインを取り合う）。開発中だけ | [proxy.rs:426](../crates/ferritron-core/src/reload/proxy.rs) |

---

## 直す順の提案

1. V1・V2: ホストごと落ちる、またはエディタが二度と開かない種類。V2 は数行。
2. G1・G2: 音が欠ける。G2 は 1 か所の順番の入れ替えで済む。
3. N1・N2: 動くものを描くエディタ（アナライザ、メーター）で必ず当たる。測ってから直す（`prepare_only` の `PlanStats` に `restarts` がある）。
4. H1: 開発用バンドルの振る舞いをリリースに合わせる。
5. V3・V4・V5: エディタの使い心地。V4 は日本語の利用者に直接かかわる。

---

## 追記: FNG の CPU レンダラ（`cpu.rs`）

2026-11-10 に追加で調べた記録です。`crates/ferritron-native-graphics/src/cpu.rs` を全部読み、速さは一時的なテスト
（release ビルド、実行後に削除済み）で測りました。番号は U。

`cpu.rs` は、GPU 版が合わせるべき**基準の絵**を作るためのもので、文書にも「GPU が無い所（CI、スクリーンショット）の代わり」とあります。
読んだ範囲では、絵の正しさにかかわる誤りは見つかりませんでした（`Op` は全種類を扱い、`_` で取りこぼしていない。不透明度は `Scene` 側で 0〜1 に切ってある。
矩形のクリップを整数ピクセルに丸めるのは GPU 版も同じ）。問題は主に、**エディタを描く代わり（要望 F30）として使うには遅い**ことです。

### 測った速さ

エディタに似た絵（背景、角丸の枠、つまみ 24 個と針、800 点のスペクトルの線）を、同じ `CpuRenderer` で 20 フレーム描いた時間です。
マシンは `docs/performance.md` と同じ（Intel Core i7-10700KF、Windows 10）。

| 条件 | 物理ピクセル | 1 フレーム（最良 / 平均） |
|---|---|---|
| 基本 | 800 × 400、倍率 1 | 6.5 / 8.5 ms |
| 基本 | 1600 × 800、倍率 2 | 22.7 / 26.8 ms |
| 基本 + 大きな影 1 つ（ぼかし 24） | 1600 × 800、倍率 2 | 82.0 / 85.3 ms |
| 基本 + 不透明度のグループ 1 つ | 1600 × 800、倍率 2 | 27.7 / 29.8 ms |
| 基本 + 不透明度のグループ 8 つ | 1600 × 800、倍率 2 | 63.6 / 71.4 ms |
| 基本 | 2560 × 1440、倍率 2 | 65.5 / 73.4 ms |

60 fps の 1 フレームは 16.7 ms です。倍率 2 の画面では、基本の絵でも間に合いません。

### U1. 1 フレームごとに全部を描き直す（中）（一部修正: f5・f6、残り: 変わった所だけ描く仕組み（ダメージ領域）は未対応）

- 場所: [cpu.rs:225](../crates/ferritron-native-graphics/src/cpu.rs)〜250（`render_with`）。
- 内容: 毎回、ピクセルあたり 16 バイト（`[f32; 4]`）の面を新しく確保し、全部の命令を描き、最後に全ピクセルを 8 ビットへ変換して、
  出力の `Vec<u8>` も新しく確保する。1 スレッドで、SIMD もない。変わった所だけ描く（ダメージ領域）仕組みもない。
  1600 × 800 では、面が 20 MB、出力が 5 MB で、どちらも毎フレーム確保して捨てる。
- 割り当てを数えるテスト（`tests/alloc.rs`）は GPU 側の準備（`Prepare`）だけを見ていて、CPU 版は対象外。

**現在の状態**: 一部直した（f5・f6）。`render_into` は、同じ大きさの 2 フレーム目から、面と出力のバッファを確保しない（`tests/alloc.rs` の `a_repeated_cpu_frame_allocates_nothing`）。線の一時配列は `StrokeScratch` としてレンダラが持つ（f6）。残り: 変わった所だけ描く仕組み（ダメージ領域）は無い。`render` と `render_with` は、出力の `RgbaImage` を毎回確保する。

### U2. 不透明度のグループが画面全体の面を確保する（中、実測）（修正済み: f5・f6）

- 場所: [cpu.rs:396](../crates/ferritron-native-graphics/src/cpu.rs)〜407。
- 内容: `PushOpacity` のたびに、描く先と同じ大きさの面を新しく確保し、中身を描いてから全ピクセルを合成する。中身が小さなボタン 1 つでも同じ。
  入れ子にすると、深さの数だけ全画面の面が並ぶ。
- 実測: 1600 × 800 で、グループ 1 つにつき約 5 ms 増える（上の表）。不透明度 1.0 でも同じ処理をする。

**現在の状態**: 直した（f5・f6）。不透明度 1.0 は直接描き、それ以外のグループは、中身の範囲（窓）だけを持つ面に描いて合成する（`cpu.rs` の `Surface` の `ensure`、`start_group`）。使わない面は、2 画面続けて使わなければ解放する（f6。単体では確かめていない）。2560 × 1440 の入れ子のグループの試験で、作業セットの最大が約 140 MB から約 76 MB になった（f6、Windows、別プロセスで交互に測定）。残り: 窓を広げる回数は測っていない。

### U3. 影をピクセルごとに計算する（中、実測）（修正済み: f5）

- 場所: [cpu.rs:474](../crates/ferritron-native-graphics/src/cpu.rs)〜504（`draw_shadow`）。
- 内容: 影の範囲の全ピクセルで、逆変換とぼかした角丸矩形の式（`shade::blurred_round_rect`）を計算する。影の内側はほとんど一定の値なのに区別しない。
  キャッシュもしない。
- 実測: 画面に近い大きさの影 1 つで、1600 × 800 のフレームが約 60 ms 長くなる（22.7 → 82.0 ms）。

**現在の状態**: 直した（f5）。影の内側は一定の値なので、縁の帯だけを式で計算する（`cpu.rs` の `ShadowShape` の内側の値）。被覆マップは、形・拡大率・中心の小数部をキーにして覚える。確かめたこと: `shade.rs` の `interior_constant_matches_the_formula_deep_inside`、`tests/cpu_reference.rs` の `a_shadow_moved_by_whole_pixels_is_the_same_picture`。回転や傾きのある変換は、従来どおり 1 画素ごとに計算する。

### U4. グリフのキャッシュ（低）（修正済み: f5）

- 場所: [cpu.rs:524](../crates/ferritron-native-graphics/src/cpu.rs)。
- 30,000 個を超えると全部消す。数で数えていてバイト数を見ないので、大きな文字が多いと消す前にメモリを多く使い、消した直後のフレームは全部のグリフを塗り直す。
- 回転・左右と上下で倍率が違う・左右反転した文字（`upright` でないもの）は、キャッシュせず、毎フレーム輪郭から塗る（538〜543 行）。

**現在の状態**: 直した（f5）。グリフ（回転などの文字と、輪郭から作るものを含む）は、32 MB の予算（`GLYPH_CACHE_BYTES`、`BudgetCache`）で覚え、古いものから消す。数はバイト数で数える。確かめたこと: テキストの参照画像と GPU との比較（`tests/text.rs`）。

### U5. 破線のたびに確保する（低）（修正済み: T26）

- 場所: [cpu.rs:321](../crates/ferritron-native-graphics/src/cpu.rs)〜324。
- `std::mem::take(&mut self.flat2)` で作業用の配列を取り出してから、空になった `self.flat2` へ書くので、破線を描くたびに配列を確保し直し、
  取り出した側の容量は捨てる。作業用の配列をもう 1 つ持って入れ替えれば確保しない。

**現在の状態**: 直した（T26）。破線は `flat2` と `flat3` を入れ替えて使うので、破線の一本ごとに配列を確保しない（`cpu.rs` の `Op::StrokePath` の処理）。Planner の線にも同じ入れ替えを入れた（f6）。確かめたこと: `tests/alloc.rs` の CPU の場面（破線を含む）で、2 フレーム目から確保 0。

### U6. 大きな画像の縮小で折り返しが出る（低、両方のバックエンド）（修正済み: f6）

- CPU 版の画像の取り出しは最近傍か 4 点の線形補間だけで（[cpu.rs:149](../crates/ferritron-native-graphics/src/cpu.rs)）、GPU 版もテクスチャのミップマップを作らない
  （[gpu.rs:64](../crates/ferritron-native-graphics/src/gpu.rs) `mip_level_count: 1`）。大きな画像（背景、ノブの連番画像）を半分以下に縮めて描くと、ちらつきやモアレが出る。
- CPU 版は、標本ごとに 8 ビットから浮動小数点への変換と端の切り詰めを 4 回する（速さの面でも重い）。

**現在の状態**: 直した（f6）。画像を登録するとき、2×2 の平均で段を作る（`image.rs` の `box_levels`）。GPU はその段を使い、サンプラを線形にする。CPU は二つの段を線形に混ぜる。両方とも同じ LOD の段を選ぶ。最近傍のサンプリングは元の大きさの画像のまま。確かめたこと: `tests/mip_image.rs` の 2 件（市松模様を 1/4 に縮めた平均、GPU と CPU の一致）。残り: 段のぶん、画像のメモリは約 1/3 増える（測っていない）。

### U7. レイヤーの解像度（低）（修正済み: f5）

- 場所: [cpu.rs:548](../crates/ferritron-native-graphics/src/cpu.rs)〜601。
- レイヤーは、一番外側の倍率（`st.xf[0]`、画面の DPI）で 1 回描いて取っておき、線形補間で貼る。レイヤーを `push_transform` の拡大の中で描くとぼやける。
  平行移動に小数があると、補間で全体が少しぼやける。GPU 版が同じかどうかは確かめていない。

**現在の状態**: 直した（f5）。レイヤーは、現在の変換の最大の拡大率（`scene.rs` の `layer_scale`、`Affine::max_scale`）で描き、DPI より小さくはしない。一辺は 4096 px でおさえる（`LAYER_MAX_PX`）。確かめたこと: `cpu_reference` の `a_layer_under_a_zoom_is_drawn_sharp`、`gpu_match` の `gpu_matches_cpu_layer_under_a_zoom`。残り: f6 で、アルファ 2/255 以下の縁の画素を差の数から外した（閾値 48 を超えていた 8 画素は縁の丸めの違い。原因は直していない）。

### U8. 大きさの桁あふれ（低）（修正済み: f5）

- 場所: [cpu.rs:72](../crates/ferritron-native-graphics/src/cpu.rs)（`w * h`）、226（`size_px as i32`）、239（`w * h * 4`、`i32`）、31（`RgbaImage::pixel`、`u32`）。
- 約 23,000 × 23,000 を超える大きさで `i32` があふれる。debug では panic、release では短い出力になり、`zip` で黙って一部だけ書く。
  現実のエディタでは届かない大きさだが、スクリーンショットの道具に任意の大きさを渡すと起きる。

**現在の状態**: 直した（f5）。一辺が `MAX_SIDE`（16384）を超えると、`render_into` は `RenderError::TooLarge` を返し、`render` と `render_with` は panic する。黙って短い絵を返すことはない（`cpu.rs`）。確かめたこと: `tests/alloc.rs` の `render_into_refuses_a_bad_size_or_buffer`。

### U9. 矩形のクリップは整数ピクセルに丸める（低、両方のバックエンド）（修正済み: f5。文書に記載）

- 場所: [cpu.rs:351](../crates/ferritron-native-graphics/src/cpu.rs)〜353、[prepare.rs:569](../crates/ferritron-native-graphics/src/prepare.rs)〜572。
- 拡大と平行移動だけのとき、クリップの矩形を最も近い整数ピクセルに丸め、縁をぼかさない。両方のバックエンドで同じなので絵は一致するが、
  小数の位置で動くクリップ（スクロールする一覧、開閉するパネル）は 1 ピクセル単位で跳ぶ。文書に書かれていない。

**現在の状態**: 直した（f5）。`Scene::push_clip_rect` の文書に、矩形のクリップが整数ピクセルに丸まり、縁がぼかされず、小数の位置の動きが 1 ピクセルずつ跳ぶことを書いた（`scene.rs`）。両方のバックエンドで同じ。

### U10. 出力の形式（低）（修正済み: f5）

- 出力は毎回新しい `RgbaImage`（ストレートアルファの RGBA）。ウィンドウへ転送するには、Windows の `BGRA`・乗算済みアルファへもう一度変換して写す必要がある。
  前もって確保した出力へ書く口（`render_into`）と、転送向けの形式の指定がない。F30 のために要る（要望 F50）。

**現在の状態**: 直した（f5）。`CpuRenderer::render_into` は、呼び手の用意した `&mut [u8]` に `PixelFormat::Rgba8` か `Bgra8Premultiplied` で書く（`cpu.rs`）。ウィンドウへの転送には後者を使う。確かめたこと: `render_into_refuses_a_bad_size_or_buffer`。`Rgba8` の絵が従来の `render` と同じであることは、参照画像の試験で確かめた（f5）。
