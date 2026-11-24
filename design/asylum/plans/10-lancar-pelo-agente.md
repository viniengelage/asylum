# 10 · Lançar do build à loja, pela dock e pelo agente

**Boards:** a desenhar em `18 · Lançamentos` (fase 2): "Aba da versão — preparar versão iOS" e
"Checklist antes de enviar para review". O resto não tem UI nova.

**Continua:** plano 04 (Lançamentos) e plano 07 (build limpa, fase 3 — tool do agente).

## Ideia

Hoje a dock acompanha tudo e faz quase todas as escritas, mas o ciclo quebra em dois pontos:

1. **App Store Connect:** o Asylum não cria a versão (ex.: 2.4.0) nem escolhe o build dela.
   `release_submit_review` só funciona se isso já foi feito no site.
2. **Agente:** só tem `release_status`, `release_promote`, `release_rollout` e `release_submit_review`.
   Não dispara build, não envia build para a loja, não mexe em TestFlight, notas nem lançamento gradual.
   E nada foi testado contra as APIs reais.

Pronto quando um pedido como "lança a 2.4.0 da main nas duas lojas" vira, com uma confirmação por
escrita: build limpa → `eas submit` → versão no ASC com o build → notas → review / track → gradual.

## Fase 0 — validar o que existe com contas reais (antes de escrever código novo)

Com um app real (Trix), pela dock:

- Contas: .p8 do ASC (papel App Manager), conta de serviço do Play com acesso ao app (e
  `playdeveloperreporting` para vitals), token Expo.
- Roteiro: build limpa (`preview` ou `production`) → acompanhar no EAS → "Enviar para a loja" →
  build aparece processando no ASC / na track internal → pôr em grupo do TestFlight → promover no Play.
- Anotar cada divergência entre fixture e resposta real; corrigir parsers antes da fase 1.
- Conferir se `eas submit --non-interactive` passa só com `EXPO_TOKEN`. Hoje ele depende de
  credenciais de submit já salvas no EAS ou no `eas.json`; se falhar, a fase 1 vira obrigatória.

> Feito sem contas reais (não há credenciais nesta máquina): o roteiro continua valendo e precisa
> ser rodado no app. O que deu para validar foi o eas-cli 27.11.0, lido no pacote: `eas submit`
> **não** lê credenciais de variável de ambiente. Só lê o perfil de submit do `eas.json` (que
> expande `$VARS`) ou o que está salvo no EAS, e no iOS sem `ascAppId` recusa o modo não interativo.
> Por isso a fase 1 mudou (abaixo).

## Fase 1 — `eas submit` com as contas de Lançamentos

`clean_build::submit_build` só passa `EXPO_TOKEN`. Sem credenciais de submit no EAS/`eas.json`, o
modo não interativo falha.

- iOS: gravar o .p8 do keychain num arquivo temporário (0600, apagado ao fim da task) e passar
  `EXPO_ASC_API_KEY_PATH`, `EXPO_ASC_KEY_ID`, `EXPO_ASC_ISSUER_ID` (confirmar os nomes na doc do eas-cli).
- Android: idem com o JSON da conta de serviço. O eas-cli lê `serviceAccountKeyPath` do `eas.json`
  do projeto; se não houver variável de ambiente equivalente, a saída é exigir o campo no `eas.json`
  (com aviso na dock) em vez de reescrever o arquivo do usuário. Decidir na fase 0.
- Usar `--no-wait` e guardar o id da submissão para o `ReleasesStore` acompanhar na hora (como os ids do
  `eas build --json`).
- Segredos nunca no comando nem no log da task; só em arquivo temporário e variável de ambiente.

> Feito. Antes de enviar, a dock lê o perfil `production` do `eas.json` (com `extends`):
> - iOS sem `ascAppId` → não envia e mostra o trecho exato a colar, já com o id do app.
> - Perfil apontando para `$EXPO_ASC_API_KEY_PATH` / `$EXPO_ASC_KEY_ID` / `$EXPO_ASC_ISSUER_ID`
>   (iOS) ou `$ASYLUM_GOOGLE_SERVICE_ACCOUNT_KEY_PATH` (Android) → o Asylum grava a chave do keychain
>   num arquivo 0600 em `$TMPDIR/asylum-submit`, passa as variáveis só para a task e apaga o arquivo
>   ao fim (ou se o envio nem começar). Sem isso, o EAS usa as credenciais que ele já tem.
> - `eas submit` roda com `--no-wait`; o acompanhamento é o da dock.
> O Asylum não edita o `eas.json` do usuário.

## Fase 2 — preparar a versão no App Store Connect

Novo em `app_store_connect.rs` + `StoreWrite`:

| Escrita | API |
|---|---|
| `CreateVersion { version_string, release_type }` | `POST /v1/appStoreVersions` (platform `IOS`, relação `app`) |
| `RenameVersion { version_id, version_string }` | `PATCH /v1/appStoreVersions/{id}` — só existe uma versão editável por vez; se a 2.3.1 está em "Preparar para envio", renomear em vez de criar |
| `SelectBuild { version_id, build_id }` | `PATCH /v1/appStoreVersions/{id}/relationships/build` |
| `SetReleaseType { version_id, release_type }` | `PATCH` com `releaseType` = `MANUAL` / `AFTER_APPROVAL` / `SCHEDULED` (+ `earliestReleaseDate`) |
| `SetEncryption { build_id, uses_non_exempt }` | `PATCH /v1/builds/{id}` (`usesNonExemptEncryption`) quando o build chega sem a declaração |

**Checklist antes de enviar para review** (lido, sem escrita), mostrado na aba da versão e devolvido
pela tool: versão existe e está editável · build escolhido e processado (`VALID`) · criptografia
declarada · What's New preenchido em todos os idiomas (obrigatório em atualização) · tipo de
lançamento. `release_submit_review` passa a recusar com a lista do que falta, em vez de erro da Apple.

UI (desenhar antes): na coluna iOS da aba da versão, bloco "Preparar versão" com campo da versão
(pré-preenchido do `appVersion` do build do EAS), seletor de build (builds `VALID` dessa versão),
tipo de lançamento e o checklist com status pill por item. Botão "Enviar para review" só liga com o
checklist verde.

Testes: corpos das requisições e parser com fixtures; lógica "criar × renomear" e o checklist.

> Feito, sem board novo: o bloco "Versão na App Store" segue o design system (chips, segmented,
> status pill) e o texto deste plano. Desvio: tipo de lançamento só Manual / Após aprovação;
> "agendado" fica no App Store Connect (precisa de data). O botão "Enviar para review" some
> enquanto o checklist tem pendência. O bloco não aparece em versão sem nenhum build no ASC.

## Fase 3 — tools do agente para o ciclo inteiro

Cada escrita é `ToolAccess::AlwaysConfirm`, com o mesmo resumo que a dock mostraria.

| Tool | Faz | Reusa |
|---|---|---|
| `eas_build` | build limpa na nuvem (`--no-wait`) de `branch`, `profile`, `platform`; devolve os ids | `clean_build_script`, `branch_state` |
| `eas_submit` | envia um build do EAS para a loja | `submit_command` + fase 1 |
| `release_prepare_version` | cria/renomeia a versão iOS, escolhe o build (o mais novo `VALID` da versão, ou `build_number`) e o tipo de lançamento | fase 2 |
| `release_notes` | What's New por idioma (iOS) / release notes da track (Android) | `StoreWrite::VersionNotes`, Play |
| `release_testflight` | põe build em grupo + What to Test + beta review opcional | escritas da fase 2 do plano 04 |
| `release_phased` | ativa/pausa/retoma/completa o gradual iOS; libera versão aprovada | escritas da fase 3 do plano 04 |
| `release_wait` (leitura) | espera até um build/submissão/processamento mudar de estado, no máximo 10 min; devolve o estado novo | `ReleasesStore::refresh` |

Pontos de desenho:

- **Sem modais para o agente.** As perguntas do modal da build limpa viram parâmetros:
  branch fora do origin só com `allow_unpushed: true`; branch à frente do origin avisa no resumo;
  `production` aparece em destaque no resumo da confirmação. O agente nunca faz push.
- **Só nuvem.** `--local` fica para a dock (leva dezenas de minutos e prende a máquina).
- **Onde roda.** As tools recebem o `Project`, não o `Workspace`. Achar o workspace da janela cujo
  `project()` é esse e usar `schedule_resolved_task_with_completion`, para a build aparecer em
  Execuções como pela dock; a tool espera a task acabar e lê o `--json`. Sem janela, recusa.
- Extrair do `CleanBuildModal::start` a montagem do `CleanBuildPlan` para uma função usada pelo modal
  e pela tool, em vez de duplicar.
- `release_status` passa a citar o checklist da fase 2 e a próxima ação sugerida por plataforma.

> Feito: `eas_build`, `eas_submit`, `release_prepare_version`, `release_notes`,
> `release_testflight`, `release_phased`, `release_wait`; `release_status` mostra versão/builds
> prontos/checklist e `release_submit_review` recusa com a lista do que falta. iOS: notas só nos
> idiomas que a versão já tem (criar idioma fica no ASC).

## Fase 4 (opcional) — agente "Lançar versão"

Um task agent pronto em Agentes (`task_agents`), com o roteiro: status → build → esperar → submit →
esperar processamento → preparar versão → notas → review / track internal → (depois) gradual.
Só prompt e toolkit `releases`; nenhuma confirmação é pulada.

> Feito: ação `app_releases::CreateReleaseAgent` (botão ✦ no cabeçalho da dock e paleta) grava
> `lancar-versao.md` nos agentes pessoais (não sobrescreve um editado) e abre o arquivo; `/lancar`.

## Riscos e limites das lojas

- **Primeiro envio Android é manual:** o Play não aceita o primeiro AAB de um app pela API. O app
  precisa já ter um release no Console.
- **App novo no Play** só aceita releases com status `draft` até a primeira publicação manual.
- **Processamento do ASC** leva de 5 a 30 min depois do submit; `release_prepare_version` sem build
  `VALID` responde "ainda processando" em vez de falhar.
- **Uma versão editável por vez** no ASC (daí renomear).
- **Review do Play** continua invisível na API (plano 04).
- Rate limit da ASC: `release_wait` usa o mesmo intervalo do polling (não abaixo de 30 s).

## Ordem

0 → 1 → 2 → 3 → 4. A fase 0 não tem código, mas é ela que diz se as fases 1 e 2 estão certas;
sem ela, as tools do agente seriam construídas sobre clientes que só conhecem fixtures.
