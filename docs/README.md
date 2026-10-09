# cargo-wk

O `cargo-wk` compila páginas `.wk`: arquivos HTML que podem incluir Rust em
blocos `<script lang="rs">`. Funções Rust auxiliares podem ficar em arquivos
`.wk.rs`.

## Destaque de sintaxe no VS Code

A extensão em `editors/vscode-wk` associa arquivos `.wk` à linguagem WK,
destaca o HTML e usa a gramática Rust dentro de blocos `<script lang="rs">`.
Para destacar o Rust embutido, a extensão depende do plugin `rust-analyzer`
para VS Code.

Para empacotar e instalar a extensão, execute da raiz do repositório:

```sh
cd editors/vscode-wk
npx --yes @vscode/vsce package
code --install-extension wk-syntax-0.2.0.vsix
```

## Novo projeto

```sh
cargo wk new meu-app   # cria a pasta meu-app/
cargo wk init             # cria o projeto na pasta atual, com o nome dela
```

## Desenvolvimento

```sh
cargo wk run [--port 8080]   # compila, serve dist/ e recompila a cada mudança
```

## Exemples

para rodar os exemplos, rode

```sh
cargo install --path .
cd example/minimal
cargo wk build
(cd dist && python3 -m http.server)
```
