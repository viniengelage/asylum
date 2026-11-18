# 09 · Cenas (layouts nomeados de docks)

**Boards:** `21 · Cenas` → "Workspace — Cena Mobile (ativa)", "Workspace — Cena Mobile empilhada
(Simulador | Agent + Terminal)", "Workspace — Editar cena (Mobile, arrastando Git)", "Statusbar — Seletor
de cenas (menu aberto)", "Modal — Nova cena", "Cenas — como a cena se ajusta à janela", "Cenas — linhas e
colunas (editar)", "Cenas — estados (slot vazio, desligado, modificada, troca)", "Specs — Cenas".

## Problema

Para cada ocasião o usuário quer um conjunto diferente de docks na tela (ex.: só simulador, terminal e
Agent; ou simulador | Agent em cima do Terminal) e hoje monta isso à mão toda vez, abrindo e fechando
docks. Os docks fixos (esquerda / baixo / devices / direita) não deixam empilhar Agent e Terminal na mesma
coluna nem esconder o editor.

## O que já existe (levantamento de 2026-11-18)

Tudo atrás de `workspace.unified_panes` (setting global, padrão `false`), em `crates/workspace/src`:

- **Slot = `Pane` com `accepts_kinds`** (`pane.rs:958-966`); `ContentKind` é string opaca (`pane.rs:400`),
  com `editor()`, `terminal()`, `panel(name)`. Persistido na coluna `panes.accepts_kinds`
  (`persistence.rs:1076`, round-trip testado em `persistence.rs:4161`).
- **Roteamento:** `pane_for_kind` (`workspace.rs:5660`), `add_item_to_kind_slot` (:5726), `set_pane_kinds`
  (:5798, um kind tem um destino só), `editor_target_pane` (:5672).
- **Esconder slot no lugar:** `toggle_kind_slot` (:5693), `set_slot_collapsed` (:5753, chama
  `Item::deactivated`, o que esconde a child window do simulador iOS), `PaneGroup::set_pane_collapsed`
  (`pane_group.rs:176`, guarda o flex).
- **Painéis donos de si:** `Workspace.panels` + `panels_by_type`; fechar a aba de um painel não derruba a
  Entity (teste `test_unified_panes_closing_a_panel_tab_keeps_the_panel`). Container (`TerminalPanel`)
  contribui com `hosted_content_kind` em vez de virar aba (`dock.rs:101`).
- **Trocar a árvore inteira:** `apply_pane_layout(root: Member)` (`workspace.rs:5947`) reaproveita os panes
  que continuam e derruba os outros. Ninguém chama ainda.
- **Layout inicial:** `build_unified_layout` (:5860) copia os docks; chamado em `zed.rs:866` e ao ligar o
  setting (`workspace.rs:2298`).
- **Árvore persistida** com eixo e flexes (`SerializedPaneGroup`, `persistence/model.rs:236`).
- **Linhas e colunas aninhadas:** `PaneGroup` já aceita eixos aninhados; soltar aba na borda de um pane
  insere coluna/linha (`add_panel_pane` deixa split ligado de propósito).

**Nunca validado no app** (memória `docks-para-panes-plano`): foco/atalhos dentro de painel em pane e
re-parent das NSViews nativas (CEF do browser, child window do simulador). Isso é o maior risco do plano
inteiro e por isso a fase 0 começa por aí.

**Testes:** `cargo test -p workspace --lib` tem falhas pré-existentes de sizing de painel do fork
(memória `testes-workspace-vermelhos-fork`); comparar com `git stash` antes de caçar bug.

## Modelo

```jsonc
// settings.json (escopo como as outras configs: todos os perfis / perfil / projeto)
"scenes": [
  {
    "id": "mobile-empilhada",          // estável; nome pode mudar
    "name": "Mobile",
    "shortcut": 2,                      // ⌃⌘2
    "layout": {
      "axis": "horizontal",
      "children": [
        { "flex": 0.25, "slot": { "kinds": ["panel:DevicesPanel"] } },
        { "flex": 0.75, "axis": "vertical", "children": [
          { "flex": 0.62, "slot": { "kinds": ["panel:AgentPanel"] } },
          { "flex": 0.38, "slot": { "kinds": ["terminal"], "principal": true } }
        ]}
      ]
    }
  }
]
```

- **`SceneLayout`** = árvore `Split { axis, children: [(flex, node)] } | Slot { kinds, principal }`, a
  mesma forma do `SerializedPaneGroup`, sem itens. Tipo em `settings_content` (serde + JsonSchema) e
  conversão para `Member` em `workspace`.
- **Principal:** exatamente um por cena. Recebe o que não tem slot (`editor_target_pane` cai nele quando
  não há slot `editor`). Validação: sem principal → o primeiro slot com `editor`/`terminal`, senão o maior.
- **Cena ativa** é estado da janela, não setting: coluna nova `workspaces.active_scene` (TEXT, id) no
  `persistence.rs`. `None` = **Livre** (docks de hoje).
- **Mínimos** por kind numa tabela no código (`scene.rs`): Simulador/Emulador 320, Agent 360, Terminal 360
  (altura 160), Editor 480, Arquivos 200, Git 280, Agent altura 280. Kinds de fora (crates do fork) usam 240.

### Decisão estrutural: `unified_panes` deixa de ser global

Hoje `Workspace::unified_panes(cx)` lê o setting em ~10 lugares (`workspace.rs:3118, 4976, 5218, 5344,
5373, 10126, 11292, 11522`…). Com cenas, o modo vem da janela: **cena ativa ⇒ modo slots; Livre ⇒ docks**.
Trocar para `self.layout_mode()` (campo no `Workspace`), mantendo o setting só como "abrir janelas novas
já numa cena" até ser removido. Isso também resolve duas janelas em cenas diferentes.

## Estado (2026-11-18)

**Fases 0 e 1 feitas**, testadas só em testes de unidade (330 do `workspace` passando); **não validadas no app**.
O código novo está em `crates/workspace/src/scene.rs`; o setting `unified_panes` foi removido (o modo de slots
agora é "a janela está numa cena").

Corrigido no código antigo de slots (revisão de 2026-11-18):
- painel de Devices, Notas e JSON Prettier não eram registrados como aba → slot vazio;
- terminais de task, do agente e do restore iam para o pane interno do `TerminalPanel` (invisível em cena);
  agora vão para o slot de terminal (`place_terminal` + `adopt_into_scene`);
- `focus_panel` do terminal escondia o slot em vez de mostrar;
- docks escondidos abriam e ativavam painéis (simulador por cima da cena): `Dock::set_suppressed`;
  `PanelEvent::Activate` em cena revela a aba;
- `remove_panel` deixava a aba viva; `remove_pane` com `unwrap()` derrubava o app se o pane já saiu da árvore;
- slot escondido ganhava espaço de volta quando a linha mudava (flex resetado) e perdia a proporção no restart;
- fechar o último item apagava o slot; `activated_item_id` nunca limpo (painel movido e trazido de volta não
  reativava); ativação dentro de slot escondido; navegação entre panes caía em slot escondido;
- layout montado duas vezes na inicialização (`build_unified_layout` + observer) — removidos.

Desvios do plano:
- "Salvar layout como nova cena" grava com nome automático ("Cena N") e próximo atalho livre; renomear é no
  `settings.json` até existir o modal Nova cena (fase 3).
- Cenas padrão: Código, Mobile, Mobile empilhada (Simulador | Agent / Terminal) e Revisão de PR.
- Em cena, o chip substitui os três botões de dock; em Livre o chip aparece só como ícone ao lado deles.
- Linhas e colunas já funcionam arrastando abas para a borda dos panes (o que o modo de edição da fase 2 vai
  deixar explícito) + "Atualizar cena".

Falta: fase 2 (overlay de edição), fase 3 (ajuste à janela, modal Nova cena, estados vazios), fase 4 (tool do
agente), e validar tudo no app.

## Fase 0 — trocar de cena sem UI nova

1. **Validar a base no app** antes de qualquer código novo: ligar `unified_panes`, montar à mão
   Simulador | (Agent / Terminal) arrastando abas, e conferir: child window do simulador acompanha o
   slot e some ao esconder; browser (WebKit/CEF) sobrevive a mover de slot; foco e atalhos do Agent e do
   terminal dentro do pane; restart restaura a árvore. Bug achado aqui entra como item desta fase.
2. `layout_mode` por workspace (decisão acima) + migração das chamadas de `unified_panes(cx)`.
3. Tipos `SceneLayout`/`SceneSettings` em `settings_content` + JSON schema; leitura em
   `WorkspaceSettings::scenes`.
4. `Workspace::activate_scene(id)`:
   - monta o `Member` a partir do `SceneLayout`, **reaproveitando** o pane que já declara o mesmo conjunto
     de kinds (editores e terminais abertos continuam lá) e criando `add_panel_pane` para o resto;
   - `set_pane_kinds` em cada slot + `open_panel_in_pane` dos painéis declarados;
   - painel fora da cena: tira a aba (Entity continua em `Workspace.panels`, `deactivated` chamado);
   - item multi-instância sem slot (editor, terminal, browser): vai como aba para o principal;
   - `apply_pane_layout(root)` + flexes aplicados nos `PaneAxis`.
   - `activate_scene(None)` (Livre) volta aos docks: painéis voltam aos docks como hoje e itens dos slots
     vão para o pane central.
5. Ações `workspace::ActivateScene { id }`, `workspace::ActivateSceneByIndex(n)`, `workspace::FreeLayout`;
   atalhos **⌃⌘1–9** e **⌃⌘0** no `default-macos.json` (⌃1–9 é `pane::ActivateItem`, ⌃⌥1–9 são as
   favoritas do plano 05, ⌘⌥1–9 são as notas fixadas). Linux/Windows: `ctrl-super-N` se livre, senão só
   paleta.
6. 3 cenas de exemplo no `default.json` (Código, Mobile, Revisão de PR), apagáveis.
7. Testes: aplicar cena reaproveita panes e não derruba Entity de painel; item sem slot cai no principal;
   ida e volta Livre ↔ cena mantém editores; persistência de `active_scene`.

## Fase 1 — seletor na status bar + cena modificada

1. Chip da cena na zona `StatusItemZone::Docks` (`status_bar.rs:273`): ícone, nome, atalho, chevron. Em
   cena, os três toggles de dock (`PanelButtons`, `dock.rs:478`) somem; em Livre continuam ao lado do chip.
2. Popover `ContextMenu::sectioned()` (board "Seletor"): CENAS com miniatura + descrição (kinds) + atalho,
   Livre, e ações Editar / Salvar o layout atual como cena / Gerenciar cenas (abre a página de
   configurações). Miniatura = helper novo `scene_thumbnail(layout)` em `ui::form_controls` (desenha a
   árvore em 44×28, até 3 níveis).
3. **Cena modificada:** depois de arrastar aba entre slots, redimensionar divisória ou fechar slot,
   comparar a árvore atual (eixo, flexes arredondados a 2%, kinds) com a cena → ponto âmbar no chip e
   popover "Você mudou a cena" com **Atualizar cena** (grava no escopo de onde a cena veio),
   **Salvar como nova…** e **Descartar** (reaplica). Duplo clique na divisória volta ao flex salvo.
4. Toast de troca ("Cena Código · Simulador e Agent continuam rodando", ação Voltar) só na primeira troca
   da sessão.
5. "Salvar o layout atual como cena…" serializa a árvore atual → abre o modal da fase 3 já preenchido
   (até lá, nome via prompt simples).
6. i18n (`i18n::t!`) de tudo; helpers novos: `scene_chip`, `scene_thumbnail`.

## Fase 2 — modo de edição (colunas e linhas)

1. `Workspace.scene_editor: Option<SceneEditor>`; entra por ⌃⌥E, pelo seletor ou por "Criar e editar".
   Overlay por slot desenhado no `render` do workspace usando `PaneGroup::bounding_box_for_pane`
   (`pane_group.rs:99`): fundo escurecido, borda tracejada, card com kind, proporção, chips de conteúdo,
   `+ Conteúdo`, dividir em colunas / em linhas / remover (teclas `|`, `-`, `⌫` só no modo de edição;
   **não** ⌘D, reservado ao dock de devices).
2. Barra de edição no lugar da status bar: nome, atalho, paleta com os kinds que não estão na cena
   (dock chip arrastável), Cancelar (esc) / Salvar cena (⌘S).
3. Arrastar da paleta: tipo novo `DraggedSceneContent(ContentKind)` aceito pelos drop targets do `Pane`
   (mesma lógica de borda do `DraggedTab`, `pane.rs:3165-3186`): borda esquerda/direita = coluna, topo/base =
   linha, centro = juntar como aba. Remover slot: vizinho herda o flex.
4. Marcar principal pelo card (só um). Slot sem principal na cena → aviso inline no Salvar.
5. Editar só mexe na árvore de slots; conteúdo real (terminais, editores) acompanha os panes reaproveitados.
6. Teste: editar → salvar → reaplicar dá a mesma árvore; desfazer com Cancelar volta exatamente ao anterior.

## Fase 3 — ajuste à janela + Nova cena + estados

1. **Ajuste:** no layout do `PaneAxis`, se um filho ficaria abaixo do mínimo do seu kind, recolher o slot
   de menor prioridade (ordem de criação; principal nunca) como **aba do vizinho mais largo**; devolver
   quando couber de novo. Implementação: marcar o slot como "dobrado" (reaproveita `set_pane_collapsed`) e
   mover os itens com um registro de origem para voltarem. Simulador escala o device (já faz).
2. **Modal Nova cena** (`modal_surface`): nome, começar de (layout atual / modelos / em branco) com
   miniatura, atalho livre, Vale para (`segmented_control`: todos os perfis / perfil / projeto) →
   "Criar e editar" entra no modo de edição.
3. **Estados dos slots:** vazio (Empty State do próprio conteúdo, ex. "Nenhum simulador ligado · Ligar
   iPhone 16 Pro"; o slot não colapsa em cena — ajustar `collapse_empty_slots` para não agir em slot de
   cena); dock desligado no perfil ("Ligar no perfil" / "Tirar da cena").
4. Página "Cenas" nas configurações (lista, renomear, reordenar atalhos, apagar).

## Fase 4 — agente

Tool `scene_activate { name }` (e `scene_list`) no toolkit de workspace, para "abre a cena Mobile e roda no
iPhone". Combina com `run_task` do plano 05.

## Fora do escopo / depois

- Cena por projeto aberta automaticamente ao abrir o projeto (dá para fazer com "Vale para: este projeto"
  + `active_scene` salvo; avaliar depois de usar).
- Ícone próprio `asylum / scene` (hoje `split_alt`).
- Windows/Linux: atalhos e o simulador não se aplicam; cenas funcionam igual.

## Riscos

- Child window do simulador e NSView do browser ao trocar de slot/cena (fase 0, item 1).
- `apply_pane_layout` derruba panes que não estão na árvore: garantir que nenhum item vive só neles
  (mover antes de aplicar).
- Merge de upstream mexe muito em `workspace.rs`/`pane_group.rs` (memória
  `merge-upstream-armadilhas-codigo`); concentrar o código novo em `crates/workspace/src/scene.rs`.
