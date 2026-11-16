# 01 · Servidores MCP vira atalho

**Boards:** `19 · Projetos` → "Agente — menu de opções (MCP abre Ferramentas e MCP)" e
"Configurações — Projeto Trix › Ferramentas e MCP" (linhas com estado e ações).

## Problema

No menu (…) do Agent, "Servidores MCP" é um submenu que lista os servidores com check e estado,
mais "Adicionar servidor…" (abre `OpenSettingsAt { path: "context_servers" }`) e "Instalar novos
servidores…" (extensões). Não tem um lugar claro para gerenciar, e o usuário acaba editando JSON.

## Onde mexer

- `crates/agent_ui/src/agent_panel.rs` ~5907: `submenu_with_icon(t!("agent_ui.options_menu.mcp_servers"), …)`
  + `add_context_server_entries` (~linha 280, mapeia `ContextServerStatus` para texto).
- `crates/settings_ui/src/pages/mcp_servers_page.rs`: a página já existe, com status
  (`map_server_status`), botão de autenticar (`render_status_details`), toggle e popover de adicionar
  (`render_add_server_popover`).
- Helper visual: `ui::form_controls::status_pill(label, StatusTone, cx)` (já existe).

## Fase 1 — o menu (pode ir sozinha, antes de Projetos)

1. Trocar o submenu por um item simples: ícone `Server`, rótulo "Servidores MCP", à direita:
   - pill `StatusTone::Error` "erro" se algum servidor ligado está em `Error(_)`;
   - senão pill `StatusTone::Warning` "precisa entrar" se algum está em `AuthRequired` /
     `ClientSecretRequired`;
   - senão texto discreto "N ligados";
   - ícone `ArrowUpRight` indicando que abre outro lugar.
   - Tooltip com os nomes dos servidores com problema.
   - Verificar se o `ContextMenu` aceita um `custom_entry` com elemento à direita; se não, usar
     `custom_entry` montando a linha inteira.
2. Clique: `OpenSettingsAt { path: "context_servers", target: None }` (vai para a página de MCP).
3. Remover "Adicionar servidor…" e "Instalar novos servidores…" do menu. Conferir que a página de MCP
   tem os dois caminhos no botão "Adicionar" (manual/URL + catálogo de extensões); se faltar o catálogo,
   adicionar ao popover um item que dispara `zed_actions::Extensions { category_filter: ContextServers }`.
4. Comando na paleta "agent: manage MCP servers" (`actions!`) apontando para o mesmo lugar.
5. i18n: remover chaves mortas `agent_ui.options_menu.add_server` / `install_servers`, criar as novas
   (`mcp_summary.enabled`, `mcp_summary.error`, `mcp_summary.needs_auth`).

## Fase 2 — linhas da página de MCP iguais ao board

- Cada linha: nome, comando/URL em mono `text/subtle`, `status_pill` (rodando · sob demanda ·
  precisa entrar · erro), ação contextual (**Entrar** para auth, **Ver erro** com log + Reiniciar),
  toggle. Desligado não mostra pill.
- Mapear `ContextServerStatus` → (texto, `StatusTone`) num único lugar e reusar no menu (fase 1).
- O agrupamento "MCP do perfil" / "Só deste projeto" e o seletor de nível vêm com o plano 03
  (Projetos, fase 2). Até lá a página continua mostrando o nível atual (perfil).

## Pronto quando

- Menu do Agent abre direto a página certa; o pill aparece só com problema.
- Nenhum caminho do menu leva ao JSON.
- Comparado com o board lado a lado.

## Riscos / dúvidas

- `~/.config/zed/settings.json` antigo ainda lista MCPs mas não é lido; vale um aviso único
  ("encontramos MCPs no arquivo antigo do Zed — importar?") — fica no plano 03, fase 1 (migração).
