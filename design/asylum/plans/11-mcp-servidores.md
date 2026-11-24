# 11 · Servidores MCP: lista, detalhe e adicionar

**Boards:** `23 · MCP`:

- "Configurações — MCP (lista e detalhe)";
- "… (adicionar servidor local)";
- "Modal — Colar JSON de MCP";
- "Menu — Adicionar servidor";
- "… (servidor com erro)";
- "… (nenhum servidor)";
- "Specs — MCP".

**Substitui:** a página herdada do Zed (lista de `AiSettingItem` + formulário em subpágina) em
Configurações › IA › MCP.

## O que muda

- **Lista e detalhe lado a lado:**
  - lista à esquerda, com busca e filtro Todos · Ligados · Com problema;
  - grupos "Do perfil" e "Só deste projeto";
  - em cada linha, ponto de estado, comando ou URL em mono, pill quando não está rodando e toggle.
- **Detalhe:**
  - nome, pill e invocação;
  - ações Reiniciar, Editar, … (Duplicar, Mover para o projeto/perfil, Excluir/Desinstalar) e toggle;
  - conexão, variáveis ou headers com valores mascarados, timeout e onde fica (Abrir no JSON);
  - ferramentas do servidor via `tools/list`;
  - erro (Copiar erro) e Entrar/Sair para OAuth.
- **Adicionar e editar:**
  - usam o mesmo formulário, no painel de detalhe, com Local | Remoto;
  - o comando fica numa linha só, dividida como no shell (`shlex`);
  - linhas chave-valor com olho;
  - "Salvar em" perfil ou projeto, quando há projeto Asylum aberto.
- **Colar JSON:**
  - aceita `mcpServers` (Claude/Cursor), `servers` (VS Code), `context_servers` ou um mapa solto;
  - lista os servidores com checkbox e avisa quem já existe e será substituído.

## Feito

Neste plano (commit abaixo), em `crates/settings_ui/src/pages/mcp_servers_page.rs`. Não validado no app.

Desvios do board:

- Não mostra "ligado há N min" nem o e-mail da conta OAuth: o store não guarda esses dados.
- O client secret de OAuth continua sendo pedido pelo painel do agente.
