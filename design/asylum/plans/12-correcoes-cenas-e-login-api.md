# 12 · Correções: cenas e login da dock de API

Anotado em 2026-11-26; implementado em 2026-11-27 (A, B, C1–C6, D1–D3, D7 parcial). Não validado no app.

**Ficou de fora:** D4 (os 1,5 s no `device_open` não têm causa medida; o que mudou é não reconectar
a um simulador morto), D5/D6 (reproduzir as quedas do simulador), a borda de grupos aninhados no B, e
"device ligado por fora vira aba do dock" quando a dock de iOS nem foi aberta (só funciona depois de
uma queda, pelo vigia que espera o simulador voltar). Escolher `server` por ambiente sem variável
também ficou de fora; a saída é `{{authUrl}}` no `servers` ou `auth.login.base_url`.

## A. Descartar mudanças da cena não faz nada

**Causa (confirmada no código):** em `render_scene_menu` (`crates/workspace/src/scene.rs`, ~l. 1000–1050) as
entradas "Atualizar cena", "Descartar mudanças", "Livre" e "Salvar como nova cena" são
`ContextMenuEntry::new(..).action(Box::new(..))` sem `.handler(..)`. Em `ContextMenuEntry` o `action` serve só
para mostrar o atalho; o clique chama o `handler`, que por padrão é vazio
(`crates/ui/src/components/context_menu.rs:148`, `confirm` em :1099). Ou seja, o clique fecha o menu e não
despacha `ResetScene`. O mesmo vale para as outras três entradas (provavelmente só funcionam pelo atalho).

**Correção:**
1. Nessas quatro entradas, adicionar `.handler(|window, cx| window.dispatch_action(Action.boxed_clone(), cx))`
   (mantendo o `.action` para o atalho), ou trocar por `workspace_handle.update(..)` como as entradas das cenas.
2. Conferir que `ResetScene` (= `activate_scene` com o mesmo id) volta mesmo ao layout salvo quando o usuário
   criou splits novos ou arrastou um item de slot: os panes extras saem em `apply_pane_layout`, mas o
   `assign_slot_panes` casa slot↔pane por `accepts_kinds`; se o drag mudou os kinds do pane, o casamento pode
   escolher o pane errado. Teste: aplicar cena → dividir/arrastar → descartar → `active_scene_is_modified == false`.
3. Teste de GPUI clicando a entrada do menu (não só despachando a ação), para pegar regressão do handler.

## B. Não dá para mover uma parte para uma coluna que ocupa todas as linhas

**Causa:** o arrastar só divide o pane sob o cursor (`Pane::handle_drag_move`, `crates/workspace/src/pane.rs:4096`
→ `drag_split_direction` → `split` do pane alvo). Com `A / B` empilhados, soltar à direita de `A` gera
`(A | C) / B`; não existe alvo de drop que gere `(A / B) | C`. O `PaneGroup` já sabe fazer isso
(`insert_pane_at_border` e `move_to_border`, `pane_group.rs:124` e :192; ações `MovePaneLeft/Right/Up/Down`
em `workspace.rs:9638`), mas só pela paleta/atalho e só para o pane ativo inteiro.

**Correção:**
1. Faixa de drop nas bordas do `center` (não de cada pane): quando o cursor de um drag de aba/pane estiver a
   poucos px da borda externa da área central, mostrar o realce ocupando a borda inteira; ao soltar, criar pane
   novo com `insert_pane_at_border` e mover o item para ele (ou `move_to_border` se o drag for do pane todo).
   Precisa ter prioridade sobre o split do pane da borda (o drop do pane não pode consumir o evento).
2. Generalizar para bordas de cada `PaneAxis` aninhado (ex.: `Simulador | (Agent / Terminal)` → coluna que
   cobre só o grupo da direita): alvo na borda do eixo, inserindo como irmão do eixo no pai. Pode ficar
   para depois; a borda da janela resolve o caso relatado.
3. Em cena, o pane novo herda `accepts_kinds` do item, para a cena ficar marcada como modificada e
   "Atualizar cena" gravar a coluna.
4. Testes em `pane_group`/workspace: `(A/B)` + drop na borda direita → raiz `Horizontal[Vertical[A,B], C]`.

## C. Dock de API: login Keycloak sem proxy

Relato: `/Users/viniengelage/Projects/Trx/asylum-dock-api-login-keycloak.md`. Parte já existe desde ea4ac82d02
(`auth.login.format: "form"|"json"`, `servers` da operação/caminho com `{{authUrl}}`, corpo form-urlencoded);
as falhas vêm das brechas abaixo (tudo em `crates/api_client/src/`).

1. **Corpo vai em JSON (415).** `auth_draft` (collection.rs:978) usa o tipo "preferido" do spec, que é JSON
   primeiro quando existe (spec.rs:1066–1101). O Keycloak declara form **e** json, então JSON vence, em
   qualquer ordem. Correção: para o login, `format: None` + urlencoded entre os tipos declarados → form
   (respeitar a ordem do spec em vez de priorizar JSON); `format` explícito continua mandando. Documentar o
   campo `format` no exemplo da coleção.
2. **`api_send login` explícito segue outro caminho.** agent_toolkit.rs:291–333 usa `collection.draft(key)`
   (rascunho guardado, URL/corpo antigos), ignora `login.format` e `login.body`. A renovação usa `auth_draft`.
   Correção: quando o op é `auth.login.operation`, montar pelo `auth_draft` (corpo do caller, se houver,
   sobrescreve o `login.body`); um só jeito de montar URL/Content-Type. Explica o item 3 do relato.
3. **Host por ambiente.** URL = `servers.first()` (spec.rs:151–163, 1056). Com `{{authUrl}}` funciona via
   `fill`; sem variável, sempre o primeiro. Correção: manter `{{var}}` como caminho oficial; além disso
   `auth.login.baseUrl` opcional na coleção (aceita `{{authUrl}}`) que ganha do `servers`. Escolher server por
   ambiente sem variável fica fora (adivinhação).
4. **`{{authUrl}}` sem valor / host errado no ambiente certo.** O agente tem `Collection`s próprias
   (`AgentCollections`, agent_toolkit.rs:23–71), separadas das do painel (panel.rs:307): ambiente ativo,
   sessão e rascunhos podem divergir do que o usuário vê na dock. Correção: compartilhar a entidade
   `Collection` entre painel e agente (registry por caminho), ou no mínimo ler o ambiente ativo do painel.
5. **Edição só vale na chamada seguinte.** Watcher com 300 ms + parse em background; `send` lê o que está em
   memória e não espera `spec_task`/`reload_file` (collection.rs:278–389, 685). Correção: antes de montar a
   requisição, comparar mtime do spec/coleção com o carregado e, se mudou, recarregar e aguardar; descartar
   rascunhos cujo op mudou.
6. **Resultado não mostra o login.** `run_step` (collection.rs:1494) guarda só o path (tira o host) e nada de
   Content-Type. Correção: `TimelineStep` ganha URL completa (mascarada) e Content-Type; saída do agente
   (agent_toolkit.rs:335–394) e `render_timeline` (request_view.rs:2859) mostram; falha da renovação aparece
   como passo, não só o 401 final.

Itens antigos do relato já cobertos no código: `form` sem arquivo vira urlencoded (salvo op documentado como
multipart), Content-Type da chamada ganha do da coleção (send.rs:583–599), `{{secret.*}}` em chamada avulsa.
Revalidar no teste de aceite.

**Testes:** estender `logs_in_on_the_operation_server_with_a_form_body` (collection.rs:2203) com spec que declara
form + json (espera form), `api_send login` explícito, troca de ambiente prod/homolog sem editar nada, edição do
spec seguida de chamada imediata. Aceite final: os 5 passos do relato, sem o proxy.

## D. `device_open_url` no iOS "quebra e trava o app"

Investigado em 2026-11-27 com os logs da sessão de 26/11 (perfil organizadin, thread "Organizadin Design System
Development Setup", iPhone 16e `F4F2458A…`).

**O que os logs mostram:**
- 22:48:30: o próprio simulador caiu (`CoreSimulatorService: bound port died` / `launchd_sim termination`). Nessa hora
  o agente só editava arquivos do app RN; a última tool de device tinha sido bem antes. A chamada seguinte,
  `device_open_url organizadin://transactions`, já achou o device em `Shutdown` (erro 405), e o
  `device_screenshot` deu ETIMEDOUT. Ou seja, nesse caso o openurl **revelou** a queda, não a causou.
- 21:52: outra queda, de outro tipo: SpringBoard, Organizadin e SafariViewService morreram juntos com SIGSEGV em
  `XCTAutomationSession initWithAccessibilityFramework` (automação de acessibilidade dentro do simulador). O fork
  não usa XCTest/AX. O DeviceHub (Xcode 27) estava aberto e é o suspeito, mas não está provado.
- Depois da queda, o Asylum não percebe: o sync de pasteboard (`sidebar.rs` ~12140) continua rodando
  `xcrun simctl pbpaste` a cada intervalo, para sempre (462 falhas no log até o reinício).
- 22:18 e 22:55: travamentos do thread principal em `device_open` (`device_toolkit.rs:1127`), de 297 ms e 1,5 s. O de
  22:55 foi logo depois da queda. Às 22:55 o log para, e o app só volta a registrar algo quando foi reiniciado (23:56).
  Não dá para afirmar pelo log se o app ficou congelado esse tempo todo (o detector só registra a tarefa quando ela
  termina), mas bate com "trava o app".
- Em todo o período, `simctl` é chamado ~1×/s (pasteboard), e as screenshots fazem a memória oscilar ±120 MiB a cada 30 s.

**Correções planejadas:**
1. Detectar simulador morto: observar o estado do device (notificação do SimDevice / `simctl list` quando um comando
   der 405 "Shutdown") e marcar a instância como caída. A dock mostra "Simulador encerrado · Reabrir", e não uma tela
   congelada.
2. Parar o sync de pasteboard ao primeiro erro de "not booted"/"Shutdown", e religar só quando o device voltar.
   Aproveitar para trocar o polling de `pbpaste` por algo mais barato (só ao focar a janela do device, ou intervalo maior).
3. Tools de device: timeout em todo `xcrun simctl` (`openurl`, `io screenshot`, `pbcopy`), e erro claro quando o device
   não está bootado ("o simulador caiu; rode device_open"), sem deixar o agente insistir.
4. Tirar do thread principal o trabalho síncrono de `open_device_for_agent` (boot/attach do SimulatorKit) que gera
   os 1,5 s em `device_open`. Antes, medir com hang trace qual chamada é.
5. Investigar a queda das 21:52 (XCTAutomationSession): reproduzir com o DeviceHub fechado e aberto. Se for o DeviceHub,
   o observer que já o esconde (`hide_device_hub_while_embedded`) pode passar a encerrá-lo quando não houver Expo
   esperando por ele.
6. Reproduzir a queda do `launchd_sim` (22:48): suspeita de reload do Metro/Expo dev client com o app em primeiro plano
   ou pressão de memória. Ver `~/Library/Logs/DiagnosticReports` e `CoreSimulator.log` na próxima ocorrência.

7. **O agente reabre o simulador sozinho, dentro do Asylum** (pedido do usuário, 2026-11-27). Hoje o `device_open`
   já faz boot e põe o simulador na dock. Mas na thread de 26/11 o agente bootou e mexeu no simulador pelo terminal
   (`npx expo run:ios -d <udid>`, `xcrun simctl openurl/terminate/io screenshot` direto). O `expo run:ios` abre o
   DeviceHub, ou seja, fora do Asylum. E depois da queda o agente não tentou reabrir.
   - Recuperação automática: quando uma tool de device (`open_url`, `tap`, `type`, `swipe`, `screenshot`, `logs`)
     encontra o device em `Shutdown`, roda o mesmo fluxo do `device_open` (boot + dock, com o mesmo timeout), refaz a
     ação uma vez e avisa no resultado ("o simulador tinha caído; reabri no Asylum"). Se falhar de novo, devolve erro
     e não entra em loop.
   - Simulador bootado por fora (terminal, Expo, Xcode) vai para a dock do Asylum: ao ver um device passar a `Booted`
     sem instância embutida, embutir na dock de Devices. O observer do DeviceHub continua escondendo a janela dele.
   - Orientação ao agente: as descrições de `device_open`/`device_list` e o prompt do toolkit dizem para abrir e bootar
     só pelas tools, e não por `simctl boot`/`open -a Simulator`/`devices://`. Para build, o `expo run:ios` continua
     liberado, já que o item acima traz o device para o Asylum.

## Ordem sugerida

A (pequeno, causa clara) → D1–D3 + D7 (o app para de travar com o simulador caído e o agente o reabre no Asylum) → C1+C2 (destravam o login) → C5 →
C3/C4 → C6 → D4–D6 → B (maior, mexe em drag/drop).
