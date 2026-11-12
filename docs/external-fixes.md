# 外部からの修正の記録

Ferritron の監査や修正の計画（`release/plans/`）とは別に、Ferritron を使うプラグインの開発中に見つけて、その場で直したものの記録です。
各項目には、どこで見つけたか、何を変えたか、どこまで確かめたかを書きます。

---

## X1. `cargo ferritron bundle --features` で VST2・AAX がバンドルされない

- **日付**: 2026-11-12
- **見つけた所**: PanStudio（Ferritron のワークスペースの外にあるプラグイン）で VST2 と AAX を試しにビルドしたとき。
- **修正した人**: PanStudio 側の作業（Claude Code のセッション）。Ferritron の監査（f1〜f11）の担当ではありません。
- **基準**: ブランチ `ferritron-2.0`、`561a64a`。コミット `aadd014`。
- **変更したファイル**: `crates/ferritron-cli/src/main.rs`（`format_metadata` を追加し、`bundle_once` から使う）。

### 症状

```bash
cargo ferritron bundle panstudio --features ferritron/vst2,ferritron/aax
```

を実行すると、`ferritron-vst2` と `ferritron-aax` はコンパイルされ、ライブラリも `VSTPluginMain` と `ACFRegisterPlugin` を出しています。
それでも `PanStudio.vst2.dll` と `PanStudio.aaxplugin` はできず、エラーも警告も出ません（VST3・CLAP・LV2・スタンドアロンだけができる）。

### 原因

`bundle_once` は、形式の一覧を `cargo metadata` の `[package.metadata.ferritron.format]` から集めます（`formats::discover`）。
`--features` の内容は `cargo build` には渡していましたが（`FERRITRON_BUNDLE_FEATURES`）、`cargo metadata` には渡していませんでした。
features なしの `cargo metadata` では、既定で無効の任意の依存（`ferritron-vst2`、`ferritron-aax`）が依存グラフに入らないので、形式として見つかりません。

Ferritron のワークスペースの中（`examples/multiband-test` など）では、ほかのメンバーがこれらのクレートを有効にしているため、features なしでも一覧に入り、問題が表に出ませんでした。
ワークスペースの外のプラグインでだけ起きます。

### 修正

`format_metadata` を足し、`--features` が指定されたときは、`cargo build` と同じ一覧を `cargo metadata --features <一覧>` に渡します。
cargo がその一覧を受け付けないときは、今までどおり features なしの `cargo metadata` を使います。
`--features` を指定しないときの動きは変わりません。

一覧に入ったのにビルドされなかった形式は、これまでどおり、ライブラリの出す関数に入口がないので外されます。

（最初は `--all-features` を試しましたが、これは対象のパッケージ自身の features だけを有効にし、依存の features は有効にしないので直りませんでした。）

### 確かめたこと（Windows）

- PanStudio: `Cargo.toml` を変えずに `bundle panstudio --features ferritron/vst2,ferritron/aax`（`AAX_SDK_DIR` を指定）で `PanStudio.vst2.dll` と `PanStudio.aaxplugin` ができた。修正前は、`Cargo.toml` の ferritron の features に `"vst2", "aax"` を書き足さないとできなかった。
- `--features` なしの `bundle panstudio`: これまでどおり VST3・CLAP・LV2・スタンドアロンの 4 つ。
- ワークスペースの中: `bundle multiband-test --features vst2-test,aax-test` で VST2 と AAX を含む 6 つができた（パッケージ自身の feature 名でも動く）。
- `cargo test -p ferritron-cli`: すべて合格。
- 修正前に作った PanStudio の VST2 は `ferritron-testhost` で読み込み、エディタの表示とパラメータの変更まで確かめた。AAX は Pro Tools がないので読み込んでいない。

### 確かめていないこと

- macOS へのクロスビルド（`--target`、xclang 経由）と `--pgo` で `--features` を使う場合。どちらも同じ `bundle_once` を通るので直っているはずですが、走らせていません。
- インストール済みの `cargo-ferritron`（`%LOCALAPPDATA%\Programs\Ferritron\bin`）は 1.2.0 のままで、この修正は入っていません。

### 同じ作業で気づいたが直していないこと

- `ferritron-testhost` に VST2 の `.dll` を渡すと、`--gui` を付けなくても、作業中のフォルダに `vst2-*.png` を 3 枚書き出す。
- VST2 では `ferritron-testhost` の `--instances 3` が効かず、1 インスタンスだけで動く。

---

## X2. `[bundle]`: 毎回のインストールと、インストール先へのショートカット

- **日付**: 2026-11-12
- **きっかけ**: PanStudio の開発者からの要望（バンドルのたびにインストール先のフォルダを開けるようにしたい、自動でインストールしたい）。PanStudio 側では PowerShell のスクリプトでショートカットを作っていたものを、Ferritron の機能にした。
- **変更した人**: PanStudio 側の作業（Claude Code のセッション）。監査（f1〜f11）の担当ではない。
- **基準**: ブランチ `ferritron-2.0`、X1 の後（`4e2582c`）。

### 追加したもの

- プロジェクトファイルの `[bundle]`（`cargo ferritron bundle` が読む。プラグインには入らないので `Manifest` には足さず、`ferritron_manifest::bundle_settings` / `BundleSettings` で読む。`parse` は形と型を確かめる）:
  - `install = true`: 毎回 `--install` と同じコピーをする。`--target` と `--pgo` のビルドではしない。書けないフォルダは警告にとどめ、バンドルは成功にする。
  - `shortcuts = true`: `target/bundled` に、バンドルした形式ごとのインストール先へのショートカットを置く。名前は `<shortcut_prefix><形式名> Plugins`（Windows は `.lnk`。PowerShell の `WScript.Shell` で一度に作る。ほかはシンボリックリンク）。同じ名前のものは置き換える。フォルダがなくても作り、警告を出す。`--target` のビルドでは作らない。
  - `shortcut_prefix`（既定 `"_"`）。ファイル名に使えない文字はエラー（`bad-file-name-part`。Editor の訳を en / ja に追加）。
- `bundle` のオプション: `--shortcuts` / `--no-shortcuts`（その回だけ）、`--no-install`（`[bundle] install` をその回だけ止める）。
- VST2 のインストール先（`ferritron-vst2` の `install = { windows = "{VSTPLUGINS}" }`）。`{VSTPLUGINS}` は環境変数 `VSTPLUGINS`、なければレジストリ `HKLM\SOFTWARE\VST` の `VSTPluginsPath`（`reg query`）、なければ `%COMMONPROGRAMFILES%\VST2`。
- `Format::install_folder()`。

### ついでに直したこと

- `--install` で、インストール先のない形式（スタンドアロン）があるとエラーで止まっていた。今は「インストール先がない」と書いて飛ばす。
- Windows では、インストール先のパスの `/` を `\` にする（表示とショートカットの中身。`C:\Program Files\Common Files/VST3` のような混ざった表示がなくなる）。

### 確かめたこと（Windows）

- `cargo test -p ferritron-cli -p ferritron-manifest` と Editor の訳のテスト: すべて合格（`[bundle]` の読み取りと誤りの検出、ショートカットを作って置き換える、`reg query` の出力の読み取り）。
- PanStudio（`[bundle] shortcuts = true`）を VST2・AAX 付きでバンドル: `_AAX Plugins.lnk`、`_CLAP Plugins.lnk`、`_LV2 Plugins.lnk`、`_VST2 Plugins.lnk`、`_VST3 Plugins.lnk` ができ、`WScript.Shell` で読み直した行き先が正しかった。VST2 はレジストリの `C:\Program Files\Vstplugins`。
- 自動インストール: `install = true` で、書けないフォルダ（VST3。管理者でない）は警告になり、バンドルは成功した。`--install` では同じ場合にエラーになった。`VSTPLUGINS` と `APPDATA` を一時フォルダに向けると、VST2 と LV2 はそこに入った。

### 試験中の事故（要確認）

自動インストールの試験で `CommonProgramFiles` を一時フォルダに向けたつもりが、**Windows は 64 ビットのプロセスを起こすときにこの変数を設定し直すため効かず**、PanStudio を実際のシステムのフォルダにインストールしてしまった（2 回）。AGENTS.md の「試験でシステムのフォルダに `--install` しない」に反する。

- `C:\Program Files\Common Files\Avid\Audio\Plug-Ins\PanStudio.aaxplugin`: 試験で新しくできたものなので、2 回とも削除した。
- `C:\Program Files\Common Files\CLAP\PanStudio.clap`: もとからあった（作成 11:08）ものを、同じソースから作った新しいビルドで上書きした（11:18）。消していない。
- VST3 は書き込みが拒否されたので変わっていない。

自動インストールを試すときは、`{COMMONPROGRAMFILES}` を使う形式（VST3、CLAP、AAX）が実際のフォルダに入る。一時フォルダで試す方法は今はない（`VSTPLUGINS`、`APPDATA` だけ変えられる）。

### 確かめていないこと

- macOS / Linux のシンボリックリンク（ビルドもしていない）。
- 管理者のシェルでの自動インストール。
- Ferritron Editor（非公開）が `[bundle]` を表示・編集できるか。`parse` は通るので、開いても壊れないはず。

---

## X3. FNG のマスクのアトラスが、幅の広いマスクでテクスチャの外に書き込む

- **日付**: 2026-11-12
- **見つけた所**: PanStudio のエディタを FNG（Ferritron Native Graphics）に移したとき。`ferritron-testhost` でエディタを開くと、毎フレーム `Queue::write_texture` の検証エラー（「Copy of X 8205..8528 would end up overrunning the bounds of the Destination texture of X size 2048」）が出て、GPU のデバイスが作り直され続けた。
- **修正した人**: PanStudio 側の作業（Claude Code のセッション）。監査（f1〜f11）の担当ではない。
- **基準**: ブランチ `ferritron-2.0`、`35542ef`。
- **変更したファイル**: `crates/ferritron-native-graphics/src/atlas.rs`。

### 原因

`Atlas::alloc` は、新しい棚（シェルフ）を作るときに、高さが空いているかしか見ていなかった。アトラスより幅の広いマスク（PanStudio では窓の幅いっぱいの影など。拡大した画面で 2048 ピクセルを超える）が来ると、幅がはみ出した棚ができ、その棚の `x` がアトラスの幅を超える。次に既存の棚を探すときの `self.size - s.x >= w` は `u32` の引き算なので、`s.x` が幅を超えていると桁あふれして大きな値になり、条件が通ってしまう。以後、その棚には `x` がアトラスの外の位置でマスクが置かれ続けた。

### 修正

- 既存の棚の空きは `s.x + w <= self.size` で調べる（引き算をしない）。
- 新しい棚は、マスクの幅がアトラスの幅に収まるときだけ作る。収まらなければ `None` を返し、呼び出し側がアトラスを大きくしてから置き直す（最大 4096。それより広いマスクはこれまでどおり `Full`）。

### 確かめたこと（Windows）

- 追加したテスト `a_mask_wider_than_the_atlas_grows_it`（最小の 1024 より 500 広いマスクの後に 323 幅のマスクを 38 個置き、どれもアトラスの中に収まる）と、`cargo test -p ferritron-native-graphics` のすべて: 合格。
- PanStudio を `ferritron-testhost` で開き、検証エラーが出ず、画面が正しく描かれることをスクリーンショットで確かめた。

### 確かめていないこと

- 4096 を超える幅のマスク（4K 以上の画面で窓いっぱいの影など）。これまでどおり置けず、そのマスクは描かれない。
