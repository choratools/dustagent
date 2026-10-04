---
id: 05_interface_and_cli_spec
title: 05. 인터페이스 및 CLI 스펙
type: spec
tags: [dustagent, cli, spec, pipe, headless, stdin-stdout]
created: 2026-10-01
updated: 2026-10-02
status: active
aliases: [CLI 스펙, 인터페이스 명세]
---

# 인터페이스 및 CLI 스펙

호출하는 에이전트가 입력과 실행 옵션을 정하고 STDOUT 결과·종료 코드를 처리한다. 실행 중 대화형 확인은 없다. 아래는 현재 dust --help와 각 하위 명령 도움말을 기준으로 작성한 실행 계약이다.

## 명령

| 명령 | 입력 | 산출 |
| --- | --- | --- |
| dust run APP [OPTIONS] [INPUT...] | 앱 이름, JSON 경로, 패키지 디렉터리 또는 .dustpkg | 완료한 최종 응답 또는 --json 실행 보고서 |
| dust acp APP [OPTIONS] | 앱 이름·매니페스트·디렉터리·.dustpkg | ACP v1 stdio JSON-RPC 세션 |
| dust new NAME DESCRIPTION | 자연어 작업 설명 | apps/NAME/app.json 및 빈 skills/; --stdout이면 JSON만 출력 |
| dust pack SOURCE [-o OUTPUT] | app.json이 있는 패키지 디렉터리 | .dustpkg 생성 후 경로 출력 |
| dust install SOURCE [--store STORE] | 로컬 디렉터리 또는 .dustpkg | 설치 후 디렉터리 경로 출력 |
| dust learn APP [--list] | 앱별 경험 기록 | 자동 조사·검토; --list이면 기록 조회 |
| dust patch --file FILE INSTRUCTION | 파일과 편집 지시 | SEARCH/REPLACE 적용; --dry-run이면 블록만 출력 |

## 실행과 파이프

```sh
dust run ./apps/coverage-reader --json "discovered=290 observed=100"
dust pack ./apps/coverage-reader
dust run ./coverage-reader-0.1.0.dustpkg "discovered=290 observed=100"
git diff --cached | dust run commit_gen
```

run은 INPUT이 없으면 STDIN을 읽는다. 옵션은 입력 문장 앞에 둔다. --resume은 원래 입력을 체크포인트에서 읽으며 새 입력을 허용하지 않는다. 모델은 --model/-m으로 지정한다. API 설정은 OPENAI_API_KEY와 선택적 OPENAI_BASE_URL을 사용한다. 둘 다 없으면 Codex 파일 인증 캐시를 읽는다. 자세한 선택·갱신 계약은 [[17_Codex_인증_및_모델_연결]]을 참고한다.

새 CLI run은 경로를 지정하지 않으면 시스템 임시 디렉터리의 고유한 `dust-run-XXXXXX/state.json`에 체크포인트를 저장한다. 경로는 stderr와 보고서의 `checkpoint_path`에 표시하고 종료 후에도 보존한다. 명시한 경로가 우선하며, 초기화 실패 시 아직 파일이 없을 수 있다. 자세한 권한·재개 가능 상태는 [[13_체크포인트_및_재개]]를 참고한다.

| run 옵션 | 의미 |
| --- | --- |
| --json | 미완료 실행도 구조화된 보고서로 STDOUT 출력 |
| --report PATH | 종료 보고서를 별도 파일로 저장 |
| --checkpoint PATH | 자동 임시 체크포인트 대신 지정 경로에 저장; 새 파일 필요 |
| --resume PATH | 안전한 체크포인트 재개; --checkpoint와 동시 사용 불가 |
| --max-turns N | 이번 호출의 모델 턴 예산 |
| --timeout-ms MS | 시작·경험 검토·모델·도구의 전체 시간 예산 |
| --tool-timeout-ms MS | 도구별 시간 제한 |
| --experience | 실행 기록과 과거 사례 검토·재사용 |
| --experience-dir PATH | 경험 저장소 지정; --experience를 포함 |

기본 출력은 완료 시에만 최종 응답을 쓴다. 진단은 STDERR로 쓴다. --json도 종료 코드를 바꾸지 않으므로 호출자는 반드시 확인해야 한다.

| 종료 코드 | 의미 |
| --- | --- |
| 0 | 실행 완료 및 설정된 검사 통과 |
| 1 | CLI·설정·파일 저장 오류 |
| 2 | 턴 예산 소진 |
| 3 | 전체 또는 도구 시간 제한 |
| 4 | 모델의 빈 최종 응답 |
| 5 | 실행 오류 |
| 6 | 결과 검사 실패 |
| 7 | 라이브러리 실행 취소; ACP에서는 prompt 응답으로 전달 |

## 패키지와 skill

설치는 선택 사항이다. 설치 이름은 기본 ~/.dustagent/packages에서 찾는다. DUST_PACKAGE_HOME으로 저장소를 바꿀 수 있다. --store로 설치했으면 같은 저장소를 DUST_PACKAGE_HOME으로 지정하거나 설치된 디렉터리 경로로 실행한다.

```sh
dust install ./coverage-reader-0.1.0.dustpkg --store ./local-packages
DUST_PACKAGE_HOME=./local-packages dust run coverage-reader "입력"
```

pack의 출력 부모 디렉터리는 미리 있어야 하며 출력은 원본 패키지 밖에 둔다. pack/install/new는 기존 대상을 덮어쓰지 않는다. 앱에 선언된 skill만 전용 읽기 도구에 표시된다. 파일 구조·접근 제한·배포 규격은 [[14_앱_패키지_및_스킬]]을 참고한다.

## 편집 명령

```sh
dust patch --file src/app.py --range 40:60 --dry-run "Convert this handler to async"
```

patch는 지시문을 명령 인자로 받는다. --range/-r는 선택적 줄 범위이며 --dry-run은 모델이 만든 블록을 출력하고 파일을 바꾸지 않는다. --diff, --pipe, --mcp 같은 초기 설계 플래그와 IDE JSON-IPC 전용 프로토콜은 현재 구현하지 않았다. IDE는 run의 STDIN/STDOUT, --json 보고서 또는 ACP 클라이언트로 dust acp를 사용할 수 있다. ACP 계약은 [[16_ACP_인터페이스]]를 참고한다.

관련 문서: [[10_dust_new_스캐폴딩]], [[11_경험_기반_자기강화]], [[12_실행_종료_및_시간_예산]], [[13_체크포인트_및_재개]], [[14_앱_패키지_및_스킬]].
