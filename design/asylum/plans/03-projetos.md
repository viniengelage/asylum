# 03 · Projetos

**Boards:** `19 · Projetos` → "Início — escolher projeto (vários projetos)", "Modal — Novo projeto",
"Configurações — Projeto Trix › Ferramentas e MCP", "Configurações — Projeto Trix › Lojas e EAS",
"Specs — Projetos".

## Ideia

Projeto = uma pasta escolhida (pode conter vários repositórios) com as próprias ferramentas.
O **perfil** continua sendo quem você é (e-mail, git, contas); o **projeto** é onde você está.

```
Todos os perfis  ~/.config/asylum/settings.json            aparência, fontes, teclado, editor
└─ Perfil        profiles/<id>/config/settings.json         identidade, contas, MCP/toolkits base
   └─ Projeto    <pasta>/.asylum/settings.json + Keychain   liga/desliga herdados, soma os próprios
      └─ Pasta   <repo>/.zed/settings.json                  compat Zed: formatter, LSP, tasks
```

**Herança:**
- Listas (`context_servers`, `agent_toolkits`, agentes): união perfil + projeto, menos o que o projeto
  desligou. Desligar no projeto não mexe no perfil.
- Valores simples: o nível mais perto vence (pasta › projeto › perfil › todos).

## Estado atual no código

- `crates/settings/src/settings_scope.rs`: `SettingsScope { Shared, Profile }`,
  `PROFILE_SCOPED_SETTINGS` (inclui `context_servers`, `agent_toolkits`), `natural_scope`.
- `crates/zed/src/zed/app_profiles.rs`: `AppProfile { id, name, color, folders, open_on_launch }`,
  `load_profiles`, `launch_folders`, `foreign_owner`, `forward_paths` (manda paths para a instância
  de outro perfil), ProfilePicker, ManageProfilesModal.
- Registro: `~/Library/Application Support/Asylum/profiles/profiles.json`.
- `crates/paths/src/paths.rs`: `config_dir`, `shared_config_dir`, `profile_settings_file_for(id)`,
  `local_settings_folder_name()` = `.zed`.
- `.asylum/` já é usado para learnings do agente (`crates/agent/src/learnings.rs`, eaab3fb822).
- Settings UI escolhe escopo em `crates/settings_ui/src/settings_ui.rs` ~6011.

## Fase 0 — modelo

1. `SettingsScope::Project` em `settings_scope.rs`; `natural_scope` continua devolvendo Profile para
   chaves de perfil — o Project só é usado quando o usuário escolhe o nível Projeto.
2. `paths`: `project_settings_file(root) = root/.asylum/settings.json` e helper `asylum_folder_name()`
   (reusar em learnings).
3. `SettingsStore`: nova camada entre perfil e locais (`.zed`), lida do projeto ativo.
   - Merge de listas: chaves com `{"enabled": false}` no projeto removem o item herdado; itens novos
     somam. Começar só com `context_servers` e `agent_toolkits`; os outros valores seguem merge normal.
   - Testes: herdar, desligar no projeto, somar no projeto, projeto sem arquivo.
4. Registro de projetos: `projects.json` ao lado de `profiles.json`
   (`{ id, name, root, profile_id, last_opened }`). Funções `load_projects`, `project_for_path`.
5. Workspace sabe qual é o projeto ativo (global `ActiveProject` por janela; um projeto por instância
   de perfil por vez, como hoje com as pastas).

**Feito em 713e0049dd:** `SettingsScope::Project`; `paths::asylum_project_settings_file` / `asylum_projects_file`;
camada `set_asylum_project_settings` no `SettingsStore` entre perfil e servidor/`.zed`, com
`{"enabled": false}` só desligando o MCP herdado (`take_project_disabled_servers`, testes);
`update_settings_file_in_scope(Project)` grava no `.asylum/settings.json`. Registro `projects.json`
(`crates/zed/src/zed/asylum_projects.rs`), projeto ativo = projeto da janela focada (registro ou pasta com
`.asylum/settings.json`). Testes do `zed` não rodados (binário de teste pesado demais para o disco); os do
`settings` passam.

## Fase 1 — abrir e trocar

1. Abertura do app (`app_profiles::init` / `launch_folders`):
   - 0 projetos → boas-vindas (Novo projeto · Abrir pasta avulsa);
   - 1 projeto → abre direto;
   - 2+ → janela "Escolher projeto" (board "Início"): agrupado por perfil, ⌘1–9, busca,
     "Abrir direto o último projeto" (checkbox, começa **desligado**).
   - Projeto de outro perfil → `forward_paths` para a instância desse perfil.
2. Modal "Novo projeto": pasta, nome, perfil; "Achamos na pasta" (repo git, app.json, docker-compose)
   liga Repo/Lançamentos/Docker; "Herdar tudo do perfil" × "Começar vazio"; cria `.asylum/settings.json`.
3. Seletor na barra lateral (topo da ilha do projeto: "Trix · Trabalho") + ⌘⌥O (conferir conflito,
   atalho no keymap JSON).
4. Migração única:
   - cada pasta de `profiles.json` vira projeto daquele perfil; pastas irmãs sugerem um projeto
     na pasta pai (ex.: `~/Projects/Trx` com `trix_frontend_app` + `trix_backend_core`) — perguntar
     uma vez, com prévia;
   - `open_on_launch` sai do modelo;
   - `~/.config/zed/settings.json` com `context_servers` que não existem no perfil → aviso
     "importar para o perfil X?" (uma vez).

**Feito em 713e0049dd (parcial):** `projects::OpenProject` (⌘⌥O, picker com busca, projeto de outro perfil
vai para a instância dele) e `projects::NewProject` (escolhe a pasta, cria `.asylum/settings.json`, registra no
perfil atual). Migração: na primeira vez, as pastas de cada perfil viram projetos no registro (sem escrever
nada nas pastas). **Fica para depois:** janela "Escolher projeto" na abertura do app, modal completo
(nome, perfil, "Achamos na pasta", herdar × vazio), seletor na barra lateral, sugestão de pasta pai e aviso
do `~/.config/zed/settings.json`.

## Fase 2 — Ferramentas e MCP

1. Página "Ferramentas e MCP" no grupo "PROJETO · <nome>" das Configurações:
   - seletor de nível (Todos os perfis · Perfil · Projeto) no topo;
   - "MCP do perfil" com toggle **por projeto** e estado (plano 01, fase 2);
   - "Só deste projeto" com "Adicionar MCP"; segredos no Keychain do projeto;
   - "Ferramentas do Asylum" (toolkits) com chip de origem (perfil/projeto);
   - card "Onde cada coisa fica" com os caminhos reais.
2. Menu de ações de um MCP: mover entre perfil e projeto sem recriar.
3. Scope Chip: `Type=Projeto` passa a significar o projeto Asylum; `.zed` ganha variante "Pasta"
   (`ui::form_controls::scope_chip`, ~linha 379) — atualizar o componente no `.fig` também.

**Feito em 713e0049dd (parcial):** na página de MCP, com projeto aberto, o toggle vale só para o projeto
(desligar grava `{"enabled": false}`, religar remove) e cada linha mostra o chip de origem (perfil/projeto).
**Fica para depois:** seletor de nível no topo das Configurações, "Só deste projeto" com Adicionar,
mover entre perfil e projeto, card "Onde cada coisa fica".

## Fase 3 — Contas e Lançamentos

- Contas (Apple .p8, Play service account, Expo token) com "vale para": perfil ou só este projeto.
  Segredo em `credentials_provider` com chave incluindo o escopo (`asylum/<profile>/<project?>/…`).
- Página "Lojas e EAS" no projeto (board próprio). É o que o plano 04 consome.

## Fase 4 — Agente do projeto

- Regras, modelo padrão e learnings do agente em `.asylum/` (learnings já estão lá).
- Página "Agente do projeto" no grupo PROJETO.
- Notas de projeto podem migrar de `.zed/notes/` para `.asylum/notes/` (ler as duas).

## Pronto quando (por fase)

- 0: testes de merge verdes; app abre como hoje sem nenhum projeto criado.
- 1: abrir com 1/2+ projetos se comporta como o board; migração não perde pasta nenhuma.
- 2: desligar um MCP no projeto não muda o perfil; MCP do projeto só sobe naquele projeto.
- 3/4: segredos nunca em JSON; trocar de projeto troca contas "só deste projeto".

## Riscos

- `SettingsStore` é central no Zed: manter a camada nova isolada e coberta por teste, e conferir
  merges de upstream (ver memória de armadilhas de merge).
- Instância por perfil + um projeto ativo: abrir dois projetos do mesmo perfil ao mesmo tempo fica
  fora do escopo inicial (abre o segundo em outra janela do mesmo processo com o mesmo projeto?
  decidir na fase 1 — recomendação: projeto é por janela, settings do projeto da janela focada).
