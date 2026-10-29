# RustSIP — Softphone SIP Nativo Linux

Softphone SIP moderno, corporativo e ultrarrápido desenvolvido **100% em Rust** para Linux (otimizado para Arch Linux e distribuições modernas).

Projetado como uma alternativa nativa de alta performance e código aberto ao MicroSIP e Zoiper, sem Electron, sem WebViews e sem consumo desnecessário de memória.

---

## 🚀 Destaques e Funcionalidades

- **100% Rust Nativo:** Interface gráfica com aceleração por hardware via `egui`/`eframe`.
- **Áudio Nativo PipeWire / PulseAudio:** Integração direta com `libpulse-simple` FFI em 8000 Hz S16LE Mono para latência ultrabaixa (< 20ms) e estabilidade total.
- **Detecção Automática de Dispositivos:** Seleção de fones de ouvido (Bluetooth/USB) e microfones em tempo real sem travamentos.
- **Codecs de Voz Integrados:** Implementação em Rust puro de **G.711 µ-law (PCMU)** e **G.711 A-law (PCMA)**.
- **Sinalização SIP RFC 3261:**
  - Registro e autenticação Digest Auth MD5 com controle de `nonce` e `qop=auth`.
  - Suporte a servidores **FreeSWITCH**, **Asterisk**, **OpenSIPS** e **Kamailio**.
  - Conexão tanto via **UDP** quanto via **TCP**.
  - Suporte a Early Media (**183 Session Progress**) com transição fluida para atendimento (**200 OK**).
- **RTP & DTMF:**
  - Gerenciador RTP em sockets UDP dinâmicos com RFC 4733 (`telephone-event`) e SIP INFO.
  - Sintetizador de tons DTMF locais e toques de chamada (Ringback / Ringtone).
  - Travessia de NAT via cliente **STUN (RFC 5389)** integrado.
- **Interface Corporativa & Produtividade:**
  - Design sóbrio, moderno e focado em segurança e ergonomia.
  - Teclado numérico físico e virtual com resposta tátil.
  - Gerenciamento de chamadas: Atendimento, Recusa, Mudo (*Mute*) e Espera (*Hold*).
  - Agenda de Contatos local com busca em tempo real.
  - Histórico de chamadas com filtros (Efetuadas, Recebidas, Perdidas) e rediscagem rápida.
  - Inspetor técnico de sinalização SIP ao vivo com visualização de pacotes brutos.
  - Medidores VU de entrada/saída de áudio e telemetria de pacotes RTP.

---

## 🛠️ Requisitos e Instalação

### Dependências do Sistema

No **Arch Linux**:
```bash
sudo pacman -S libpulse base-devel git
```

No **Ubuntu / Debian**:
```bash
sudo apt update
sudo apt install libpulse-dev build-essential pkg-config
```

No **Fedora**:
```bash
sudo dnf install pulseaudio-libs-devel gcc
```

---

## 📦 Compilação e Execução

Clone o repositório e compile com o Cargo:

```bash
git clone https://github.com/jp5497/sipclient.git
cd sipclient
cargo build --release
```

O binário otimizado estará disponível em:
```bash
./target/release/rustsip
```

Para executar os testes automatizados da suíte:
```bash
cargo test
```

---

## ⚙️ Configuração

Ao abrir o RustSIP, acesse a aba **Conta**:
1. **Endereço do Servidor:** IP ou domínio do seu servidor FreeSWITCH / PBX (ex: `voipotimizar.otimizartecnologia.com.br`).
2. **Usuário / Ramal:** Seu número de ramal (ex: `2003`).
3. **Senha SIP:** Sua senha do ramal.
4. **Transporte:** Selecione **UDP** ou **TCP** conforme a porta configurada no seu servidor.
5. Clique em **Salvar e Conectar**. As credenciais e preferências de áudio são salvas de forma segura em `~/.config/rustsip/config.json`.

---

## 📄 Licença

Distribuído sob a licença MIT. Consulte o arquivo [LICENSE](LICENSE) para obter mais informações.
