# ⚡ DustAgent Real-World Examples

DustAgent의 본질적인 핵심 가치인 **"가장 작고, 잡음이 없으며, 빠르고 정확한 인라인 에이전트"**를 입증하는 실제 워크플로우 예제 모음입니다.

가상/모의(Mock) 엔진이 아닌, 실제 단일 바이너리(`dust`)와 선언형 AaaA 매니페스트([`apps/`](file:///storage_0/dustagent/apps/)), 그리고 유닉스 표준 스트림(`STDIN`/`STDOUT`)을 직접 체이닝하는 구조로 구성되어 있습니다.

---

## 🧭 예제 구성 및 4대 강점 맵

```text
examples/
├── README.md                      # 본 가이드 문서
├── pipeline_demo.sh               # [강점 1 & 2] STDIN/STDOUT 유닉스 파이프라인 연쇄
└── git_precommit_autopatch.sh     # [강점 3 & 4] Git Hook 기반 무인(Zero-Interaction) 자동 패치

apps/ (특화 에이전트 매니페스트)
├── crawler.json                   # 헤드리스 웹 크롤러 & 정형 JSON 추출기 (Scoped MCP)
├── patcher.json                   # 고밀도 인라인 SEARCH/REPLACE 코드 패처
├── diagnostician.json             # 컴파일러 에러/패닉 로그 정형화 진단기
├── rust_optimizer.json            # 힙 할당 제거 및 무할당(Zero-alloc) 최적화 패처
└── commit_gen.json                # git diff 전용 Conventional Commit 생성기
```

---

## 🚀 1. 유닉스 파이프라인 연쇄 (`pipeline_demo.sh`)

```bash
./examples/pipeline_demo.sh
```

### 실제 사용 패턴:
```bash
# 1. 변경된 git diff를 파이프로 넘겨 커밋 메시지 즉시 생성 (Zero-Chatter)
git diff --cached | dust run commit_gen

# 2. 컴파일 에러를 바로 정형 진단기로 연결
cargo check 2>&1 | dust run diagnostician

# 3. 진단 결과로 특정 파일 인라인 패치 (10ms 미만)
dust patch -f src/main.rs -r 40:60 "진단 결과에 따라 타입 불일치 수정"
```

---

## 🛠️ 2. 무인 Git Pre-Commit 자동 패치 (`git_precommit_autopatch.sh`)

```bash
./examples/git_precommit_autopatch.sh
```

- **Zero-Interaction**: 사용자에게 "이 변경을 적용할까요? [y/N]"라고 묻지 않고 즉시 인라인으로 코드를 패치합니다.
- **Fast Cold Start**: 파이썬/노드 구동 지연 없이 단일 네이티브 바이너리(`dust`)가 10ms 만에 패치를 마칩니다.

---

## 📦 3. 새 에이전트 추가 방법 (빌드 0초)

새로운 에이전트를 추가할 때는 Rust 코드를 컴파일할 필요 없이, [`apps/`](file:///storage_0/dustagent/apps/) 폴더에 JSON 매니페스트 파일 하나만 생성하면 즉시 동작합니다.

예: `apps/sql_helper.json`
```json
{
  "name": "sql_helper",
  "system_prompt": "You are a database tuning expert. Output ONLY optimized SQL queries. Zero pleasantries.",
  "mcp_servers": {}
}
```

```bash
dust run sql_helper "슬로우 쿼리 로그..."
```
