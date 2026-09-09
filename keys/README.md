# Keys

## `development.key` — a development signing key / 開発用の署名鍵

**This private key is public.** It is committed to this repository on purpose,
it signs the packages this repository builds, and it is worth nothing. Anybody
reading this can sign a package that this system will install, which is exactly
what you want from a key used to test that signing works and exactly what you
must never accept from a key used for anything else.

**この秘密鍵は公開されています。** 意図的にリポジトリへコミットしており、この
リポジトリがビルドするパッケージに署名しますが、価値はありません。これを読んだ
誰もがこのシステムがインストールするパッケージに署名できます。署名機構の動作
確認用の鍵としてはそれで正しく、それ以外の用途では決して許されません。

A real signing key is generated on a machine that is not this one, kept
somewhere this repository cannot reach, and never appears in a build script. The
thing that would change to use one is a single file: `development.pub`, which is
what the installer compiles in as the key it trusts.

実運用の鍵は別のマシンで生成し、このリポジトリから到達できない場所に保管し、
ビルドスクリプトには現れません。切り替えに必要な変更は `development.pub` の
1 ファイルだけです。インストーラはこれを「信頼する鍵」として組み込みます。

## `development.pub` — the key the installer trusts

Written by `nexus-pack` from the private key the first time it is asked to
sign, and checked against it on every run afterwards. A public key that has
stopped matching its private key is a build that would ship packages nothing
can install, and it fails the build rather than the boot.

`nexus-pack` が最初の署名時に秘密鍵から生成し、以降は毎回照合します。対応が
崩れた場合はビルドが失敗します。起動時ではなくビルド時に失敗させるためです。
