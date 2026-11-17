# 07 · Build limpa a partir de uma branch (EAS)

**Boards:** `18 · Lançamentos` → "Modal — Nova build limpa (branch → EAS)",
"Build limpa — andamento (Execuções + Lançamentos)", "Build limpa — por onde entra e estados de erro",
"Specs — Build limpa por branch".

## Ideia

Um clique para gerar no EAS uma build do que está **no origin** numa branch, sem nada da máquina no
meio. É uma execução (plano 05) com etapas locais curtas; depois do envio, o acompanhamento segue na
dock Lançamentos (plano 04).

## Decisões do desenho

- "Limpa" = checkout do commit do origin numa **worktree temporária** + install do zero pelo lockfile
  (+ `--clear-cache` opcional no EAS). A worktree vale também para a build na nuvem: o EAS CLI empacota
  o diretório de onde é chamado.
- Padrão: **nuvem do EAS** (`--no-wait`); "Nesta máquina" usa `--local` na mesma worktree.
- Branch só local: oferecer "Enviar e continuar" ou "Usar commit local" (nunca faz push sozinho).
  Local à frente do origin: aviso + "Enviar antes".
- Token: sempre o token Expo de "Lojas e EAS" (perfil ou projeto), passado como `EXPO_TOKEN` só na etapa.
  Sem conta → botão desligado + "Conectar conta Expo".
- `production` pede confirmação extra com a versão.
- Build marcada "limpa" em Lançamentos via `--message "asylum clean build <sha>"`.
- Falha local (ex.: lockfile desatualizado): nada vai ao EAS, worktree fica para inspeção, "Corrigir com agente".

## Onde mexer

- Worktree: `crates/git_ui_core/src/worktree_service.rs` (`handle_create_worktree`,
  `path_for_new_linked_worktree` em `git/src/repository.rs`, setting `worktree_directory`). Criar a
  partir de `origin/<branch>` em detached HEAD, nome `build-<sha>`.
- Execução com etapas: plano 05, fase 2 (`steps`, `ready_when: exit`). As etapas: fetch+worktree,
  install (gerenciador pelo lockfile, como `languages/src/typescript.rs`), conferências (script
  `validate:expo` se existir), `eas build … --non-interactive --no-wait --json`, remover worktree.
- Saída `--json` do `eas build` dá os ids das builds → `ReleasesStore` (plano 04) começa a acompanhar
  esses ids na hora (sem esperar o polling achar).
- Entradas:
  - menu da branch no Repo ("Gerar build limpa…");
  - "Nova build" no cabeçalho da dock Lançamentos (branch atual pré-selecionada);
  - item no seletor Executar;
  - tool do agente `eas_clean_build`, com confirmação mostrando o mesmo resumo.
- Modal: branch (lista do Repo com commit/idade no origin), perfil do `eas.json` (com resumo: canal,
  distribuição, autoIncrement), plataforma, onde compila, opções, comando final e próxima versão
  (contagem remota quando `appVersionSource: remote`).

## Fases

1. Modal + etapas locais como execução + entrada no menu da branch. (Depende do plano 05, fase 2.)
2. Acompanhamento no EAS: linha "limpa" e aviso ao terminar. (Depende do plano 04, fase 1.)
3. "Enviar para a loja" a partir da build pronta (`eas submit --id`), tool do agente.

**Feito em 7624225b8b (fases 1 e 2):** `app_releases::NewCleanBuild` (botão + na dock Lançamentos e paleta)
abre o modal: branch (atual pré-preenchida), perfil do `eas.json`, plataforma, nuvem × nesta máquina, limpar
cache. Confere a branch: fora do origin → "Usar commit local" ou cancelar (nunca faz push); à frente do origin →
aviso; `production` pede confirmação. Roda como task no terminal (aparece em Execuções) um script com
worktree destacada do commit em `$TMPDIR/asylum-clean-builds/build-<sha>`, install pelo lockfile
(`--frozen-lockfile`/`npm ci`), `npx eas-cli build … --json --no-wait --message "asylum clean build <sha>"`,
e remove a worktree só no sucesso. `EXPO_TOKEN` vai no ambiente da task, vindo da conta Expo de Lançamentos.
Ao terminar, os ids do `--json` ficam salvos e a linha ganha o pill "limpa"; a dock recarrega.
Worktrees com mais de 7 dias são apagadas ao iniciar. **Desvios / fica para depois:** etapas como `steps` do
plano 05 (hoje é um script único na mesma task), `validate:expo`, entradas no menu da branch do Repo e no
seletor Executar, "Corrigir com agente" na falha, tool `eas_clean_build` do agente. **Fase 3 em 2d (mesmo dia):** "Enviar para a loja" no card da build limpa roda `npx eas-cli submit --platform … --id <build> --non-interactive` como task, depois de confirmar.

## Pronto quando

- Com mudanças locais não commitadas na branch, a build limpa sai igual à do origin (conferir commit no EAS).
- Falha de install não envia nada e deixa a worktree; sucesso apaga a worktree.
- A build aparece em Lançamentos marcada "limpa" segundos depois do envio.

## Riscos

- Install do zero leva tempo (≈1–2 min no Trix): mostrar progresso por etapa, permitir cancelar.
- Worktrees órfãs se o app fechar no meio: limpar `build-*` antigas ao iniciar.
- Monorepo/projeto em subpasta: rodar o `eas build` na pasta do app.json, não na raiz da worktree.
