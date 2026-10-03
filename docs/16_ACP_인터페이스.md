# ACP 인터페이스

DustAgent 앱을 ACP(Agent Client Protocol) 클라이언트에서 호출한다. `dust acp APP`은 기존 엔진 앞에 stdio 인터페이스를 제공한다. 작업 판단, 스킬, MCP, 결과 검사는 앱의 선언을 따른다.

```sh
dust acp ./apps/coverage-reader
dust acp ./coverage-reader-0.1.0.dustpkg --model MODEL --max-turns 15 --timeout-ms 120000 --tool-timeout-ms 15000
```

APP은 이름·JSON 매니페스트·패키지 디렉터리·아카이브를 받는다. 설치 없이 사용할 수 있다. 앱 경로는 프로세스 시작 디렉터리에서 해석하며, 아카이브 임시 리소스는 프로세스가 끝날 때까지 유지한다.

## 클라이언트 연결

클라이언트의 에이전트 실행 설정에 실행 파일과 인자를 넣는다. 아래는 실행 정보 예시이며 특정 에디터의 설정 파일 규격은 아니다.

```json
{"command":"/absolute/path/to/dust","args":["acp","/absolute/path/to/app"]}
```

프로세스 환경에 OPENAI_API_KEY와 선택적 OPENAI_BASE_URL을 전달하거나, 둘 다 생략하여 기존 Codex 파일 인증을 사용한다. 모델은 앱 설정 또는 --model을 사용한다. STDIN/STDOUT은 UTF-8 한 줄 JSON-RPC 전용이다. 진단은 STDERR로 나간다. 사람이 prompt 문자열을 STDIN에 바로 넣는 run 방식과 구분한다.

## 세션과 실행

| 요청 | 동작 |
| --- | --- |
| initialize | ACP v1 협상, 지원 기능 반환 |
| session/new | 절대경로 cwd와 mcpServers로 세션 생성 |
| session/prompt | 대화와 작업 메모를 유지하며 실행 |
| session/cancel | 진행 중인 원래 prompt를 취소하는 알림 |
| session/update | 모델 응답과 도구 시작·종료 상태 알림 |

세션마다 코어·대화·메모·작업 디렉터리를 분리한다. 같은 세션의 동시 prompt는 거부하며 서로 다른 세션은 독립 실행한다. 최대 8개 세션을 허용한다. 각 prompt의 턴·시간 예산은 새로 시작하며 초기 MCP 시작도 해당 시간에 포함한다. 이미 시작한 MCP 프로세스는 후속 prompt에서 재사용한다.

MCP와 검사기의 상대경로는 해당 세션 cwd에서 해석한다. 패키지 리소스를 실행하는 명령은 해당 cwd에서도 해석 가능한 경로로 구성한다. 전역 cwd는 변경하지 않는다. 선언된 스킬만 읽을 수 있다. 클라이언트가 보낸 MCP는 앱 선언과 command/args/env가 일치해야 하며, 빈 목록이면 앱 선언을 사용한다. 이 제한은 프로세스 샌드박스를 의미하지 않는다.

완료는 end_turn, 턴 소진은 max_turn_requests, 취소는 cancelled로 반환한다. 시간초과·실행 오류·검사 실패·blocked는 JSON-RPC 오류의 data에 ExecutionReport를 담는다. 중간 메시지는 작업 완료를 보장하지 않으므로 prompt 응답까지 확인한다. 도구 UI ID는 제공자의 반복 ID와 구분되도록 호출별로 생성한다.

모델 요청 중 취소하면 후속 대화가 가능하다. 실행한 도구나 검사기의 결과가 불확실하면 해당 세션을 차단한다. 외부 결과를 확인한 뒤 새 세션을 만든다. STDIN 종료·출력 연결 실패 시 실행을 취소하고 제한된 시간 안에 정리한다. STDIN만 닫히고 출력이 열려 있으면 진행 중 prompt의 취소 응답을 전송한다.

## 지원 범위와 제한

| 기능 | 현재 지원 |
| --- | --- |
| 텍스트 | 지원 |
| resource_link | 이름·URI와 메타데이터 전달; 자동 조회 없음 |
| 모델 응답 알림 | 모델 응답 완료 후 전송; 토큰 스트리밍 없음 |
| 도구 진행 상태 | 시작과 종료, 입력과 결과 |
| 이미지·오디오·embedded resource | 미지원, capability=false |
| session/load 및 디스크 세션 복원 | 미지원, loadSession=false |
| 클라이언트 파일·터미널·권한 요청 | 미지원 |
| 인증 UI | 미지원; 프로세스 환경 변수 사용 |
| 자동 경험 검토 | ACP 옵션으로 연결하지 않음 |

활성 세션은 메모리에 보관하며 run 체크포인트와 별개다. 원문은 별도 JSONL에 보존하고, 커진 모델용 대화는 compact한다. [[18_컨텍스트_압축_및_원문_기록]]을 참고한다. 대화는 최대 2048개 메시지/8MiB이며 한도를 넘으면 실행을 차단한다. 입력 프레임은 1MiB, 변환된 prompt는 256KiB로 제한한다. 도구 기록에는 기존 보고서 잘림 정책이 적용된다. 검사기의 과거 증거 식별에서 provider call_id만 고유하다고 가정하지 않아야 한다.

로컬 모델 HTTP fixture와 실제 Dust subprocess로 협상·연속 대화·취소·세션 분리·패키지 직접 실행·실제 검사기 cwd를 검증한다. 실제 에디터 GUI 연결과 실제 모델 품질은 별도 검증 대상이다.

공식 계약: [초기화](https://agentclientprotocol.com/protocol/v1/initialization), [세션](https://agentclientprotocol.com/protocol/v1/session-setup), [prompt와 취소](https://agentclientprotocol.com/protocol/v1/prompt-turn), [stdio](https://agentclientprotocol.com/protocol/v1/transports).
