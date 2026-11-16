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

## Fase 2 — TestFlight e tracks (escrita com confirmação)

Pôr build em grupo, What to Test, beta review, promover track, mudar rollout. Toda escrita pede
confirmação (mesmo padrão de "confirmar escritas" do banco).

## Fase 3 — lançar + agente

Notas por idioma, enviar para review, phased release, liberar versão aprovada; tools do agente
(`release_status`, `release_promote`… com confirmação).

## Fase 4 — depois do lançamento

Crashes/feedback do TestFlight, vitals do Play, avaliações.

## Pronto quando (fase 1)

- Com as três contas, a versão atual do Trix aparece com timeline correta nas duas plataformas.
- Sem contas / sem vínculo / credencial errada: estados B, C, D iguais ao board.
- Nenhuma escrita nas lojas.

## Riscos

- Play não expõe review: a linha fica "enviado" até a release aparecer na track (documentado no board).
- Limites de rate da ASC: polling só com algo andando + cache por versão.
- Segredos nunca em JSON nem em log.
