# 04 · Lançamentos (App Store Connect + Google Play + EAS)

**Boards:** `18 · Lançamentos` → "Workspace — Lançamentos (acompanhando 2.4.0)",
"Lançamentos — estados da dock e avisos" (A–F), "Configurações — Lojas e EAS", "Specs — Lançamentos".
Revisão de contas por projeto em `19 · Projetos` → "Configurações — Projeto Trix › Lojas e EAS".

## Ideia

Uma linha do tempo por versão juntando EAS, App Store Connect e Google Play. Fase 1 só lê.
Rejeição é rara: basta avisar e abrir o Resolution Center.

## De onde vem cada dado

| Fonte | Como | O que dá |
|---|---|---|
| EAS | GraphQL `https://api.expo.dev/graphql`, `EXPO_TOKEN` | builds/submits: estado, perfil, commit, appVersion, buildNumber/versionCode, duração, fila |
| App Store Connect | REST, JWT ES256 com .p8 (issuer + key id) | build processado/validade, grupos TestFlight, estado da versão (review, rejeitado, aguardando, no ar), phased release. **Sem** texto da rejeição |
| Google Play | Android Publisher v3, conta de serviço (JWT RS256 → OAuth) | releases por track, status, userFraction, notas. **Sem** estado de review nem motivo |

Ligação: appVersion + buildNumber (iOS) / versionCode (Android) do build do EAS. Build fora do EAS
aparece sem commit.

## Onde mexer

- Crate novo `crates/app_releases/` (`[lib] path = "src/app_releases.rs"`):
  - `eas.rs`, `app_store_connect.rs`, `google_play.rs` — clientes finos sobre `http_client`;
    `jsonwebtoken` (workspace, 10.0) para ES256/RS256 (`rsa` já está no workspace).
  - `model.rs` — `Release { version, build, platforms: [PlatformTimeline] }`, `Step { kind, state, detail, at }`.
  - `store.rs` — entity `ReleasesStore` por projeto: polling, cache, correlação.
  - `dock.rs` — painel "Lançamentos" no dock direito (junto de Agent/Git).
  - `release_item.rs` — aba do editor "2.4.0 (128)".
- Credenciais: `credentials_provider` (Keychain); ids em settings (perfil) e vínculo do app em
  `.asylum/settings.json` (projeto) — quando o plano 03 fase 3 existir; antes disso, no perfil.
- UI: `ui::form_controls::status_pill` já existe. Falta helper de **linha do tempo** (passo com ícone
  de estado, título, detalhe, hora; passo atual com fundo tingido) — criar em `ui` se for reusado,
  senão local no crate.

## Fase 1 — acompanhar (só leitura)

1. Clientes + testes com respostas gravadas (fixtures JSON) para os três serviços.
2. Configurações › "Lojas e EAS": contas (.p8 + issuer + key id; JSON da conta de serviço; token Expo),
   teste de conexão, vínculo do app detectado de `app.json`/`eas.json` (bundle id, package, projeto EAS).
3. `ReleasesStore`: polling a cada 2 min **só** enquanto algo anda (build, submit, review, rollout < 100%);
   senão atualiza ao abrir a dock. Sem webhooks.
4. Dock: filtro Recentes/Andando/Histórico, apps do projeto, linha por plataforma (estado + próxima ação).
5. Aba da versão: timeline iOS × Android lado a lado, grupos TestFlight, tracks do Play, notas.
6. Chip na status bar com o estado mais urgente; some quando tudo está no ar e nada anda.
7. Estados do board A–F: rejeitado (+ botão Resolution Center), perfil sem contas, projeto sem
   vínculo, credencial recusada (só a loja afetada para), chips, notificação.
8. Notificações só em: rejeitado, aprovado, build com falha, rollout parado. Token Expo avisa 14 dias
   antes de expirar.

> Feito em 1aa23d1ecf (não validado no app nem com contas reais; clientes e correlação testados só com fixtures). Desvios:
> - Board `18 · Lançamentos` não foi consultado (OpenPencil fechado na sessão); a UI segue o texto do plano e o design system.
> - "Lojas e EAS" é uma aba do editor (`app_releases::OpenAccounts`, engrenagem na dock), não uma página do `settings_ui`.
> - Ids não secretos (issuer, key id, validade do token Expo) ficam no key-value store do perfil, não no `settings.json`; segredos no keychain. Tudo passa por `accounts::accounts_source`, que é onde entra o escopo de projeto do plano 03.
> - Vínculo do app: detectado de `app.json`/`app.config.*` (raiz, um nível abaixo e `apps/*`), com correção por pasta salva no key-value store do perfil enquanto `.asylum/settings.json` não existe.
> - Validade do token Expo é digitada (opcional); a API não expõe.
> - Linha do tempo é helper local do crate. O chip da status bar só aparece depois que Lançamentos foi aberto no projeto; notificações são toasts.

## Fase 2 — TestFlight e tracks (escrita com confirmação)

Pôr build em grupo, What to Test, beta review, promover track, mudar rollout. Toda escrita pede
confirmação (mesmo padrão de "confirmar escritas" do banco).

> Feito em 8a4b953d33 (só fixtures). Toda escrita abre um modal próprio (`ConfirmWrite`) com loja, ação e alvo; nada é enviado antes de confirmar. What to Test grava por locale digitado e não carrega o texto existente. Edição do Play é descartada se qualquer passo falhar.

## Fase 3 — lançar + agente

Notas por idioma, enviar para review, phased release, liberar versão aprovada; tools do agente
(`release_status`, `release_promote`… com confirmação).

> Feito em 3cf89a90ff (só fixtures). Notas por idioma (What's New da versão na App Store; release notes da track no Play), enviar para review (reviewSubmissions), lançamento gradual (ativar/pausar/retomar/liberar para todos) e liberar versão aprovada. Tools: `release_status` (lê), `release_promote`, `release_rollout`, `release_submit_review` (sempre confirmam). Enviar para review não escolhe o build da versão: ela precisa existir no App Store Connect com o build certo.

## Fase 4 — depois do lançamento

Crashes/feedback do TestFlight, vitals do Play, avaliações.

> Feito no commit desta fase (só fixtures). Seção "Depois do lançamento" na aba da versão, carregada sob demanda: avaliações da App Store e do Play, feedback do TestFlight (crash e screenshot) e vitals do Play (crash e ANR por version code na última semana, pela Play Developer Reporting API, que exige o escopo `playdeveloperreporting` e a permissão da conta de serviço no Play Console).

## Pronto quando (fase 1)

- Com as três contas, a versão atual do Trix aparece com timeline correta nas duas plataformas.
- Sem contas / sem vínculo / credencial errada: estados B, C, D iguais ao board.
- Nenhuma escrita nas lojas.

## Riscos

- Play não expõe review: a linha fica "enviado" até a release aparecer na track (documentado no board).
- Limites de rate da ASC: polling só com algo andando + cache por versão.
- Segredos nunca em JSON nem em log.
