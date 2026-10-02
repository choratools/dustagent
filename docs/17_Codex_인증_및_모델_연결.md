# Codex 인증과 모델 연결

별도 API 설정이 없으면 Dust CLI가 기존 Codex 인증정보를 읽어 모델을 호출한다. Codex CLI·app-server·프록시를 띄우지 않는다. 실행 루프, 앱 소유 도구·스킬, 검사기, 예산과 취소는 Dust가 관리한다.

```sh
codex login
# OPENAI_API_KEY와 OPENAI_BASE_URL이 설정되지 않은 환경에서 실행한다.
dust run ./apps/coverage-reader --model gpt-6.1-sol "discovered=290 observed=100"
dust acp ./apps/coverage-reader --model gpt-6.1-sol
```

## 선택 순서

| 설정 | 연결 |
| --- | --- |
| OPENAI_API_KEY 있음 | 기존 OpenAI 호환 chat/completions |
| OPENAI_BASE_URL만 있음 | 설정 오류; 계정 인증으로 fallback하지 않음 |
| 두 변수 없음, 캐시에 OPENAI_API_KEY 있음 | 공식 OpenAI API |
| 두 변수 없음, 캐시에 ChatGPT OAuth 있음 | Codex Responses 백엔드 |
| 파일 없음·손상·필수 토큰 없음 | 오류; codex login 안내 |

빈 환경변수도 명시된 잘못된 설정으로 처리한다. Codex 파일은 CODEX_HOME/auth.json에서 읽으며 CODEX_HOME이 없으면 HOME/.codex/auth.json을 사용한다. 키링·Codex config.toml의 provider 설정은 읽지 않는다. 캐시의 토큰·계정 ID는 로그나 오류에 출력하지 않으며 Dust 저장소에 복제하지 않는다.

모델 선택은 --model, 앱 default_model, provider 기본값 순서다. API 기본은 gpt-4o-mini, Codex 기본은 현재 공개 Codex 모델 카탈로그의 gpt-6.1-sol이다. 계정별 사용 가능 모델은 다를 수 있다. 기존 앱이 gpt-4o-mini나 로컬 모델을 명시했으면 Codex 지원 모델로 변경하거나 --model을 지정해야 한다. 명시된 모델을 자동으로 바꾸지 않는다. new는 선택한 모델을 scaffold에 전달한다.

라이브러리 OpenAiProvider::new는 기존 API 키 계약을 유지한다. 자동 선택은 AutoProvider::new(Option<String>)를 사용한다. CodexProvider를 명시적으로 생성할 수도 있다.

## 요청과 갱신

ChatGPT 인증은 Codex Responses 엔드포인트에 사용한다. system은 instructions, 대화는 input, 도구 호출·결과는 function_call/function_call_output으로 변환한다. store=false로 전체 대화를 보내며, 스트리밍에서 최종 출력 항목과 response.completed를 확인한 뒤 결과를 반환한다. 스트림 중단·failed·incomplete는 완료로 처리하지 않는다. provider는 모델을 호출하며 도구를 직접 실행하지 않는다.

401을 받으면 인증 파일을 다시 확인하고, 다른 요청이 갱신했으면 새 토큰을 사용한다. 그렇지 않으면 공식 OAuth 엔드포인트로 한 번 갱신하여 원자적으로 저장하고 요청을 한 번 다시 보낸다. Unix 저장 권한은 0600이며 기존 메타데이터를 유지한다. 추론이 취소돼도 시작한 토큰 갱신은 최대 30초의 별도 제한 안에서 저장을 마친다. 인증 요청은 리디렉션을 따라가지 않는다.

프로세스 내 갱신은 직렬화한다. 별도 Codex 프로세스의 파일 변경도 저장 직전 비교로 검사하지만, 외부 프로세스와 공유하는 잠금이 없어 비교·교체 사이의 짧은 경쟁 가능성은 남는다. 갱신 실패나 충돌 시 오류를 반환하며 재로그인이 필요할 수 있다.

## 검증과 호환성

인증 파일·갱신 응답은 1MiB, SSE 프레임은 1MiB, 전체 응답은 32MiB로 제한한다. 요청에는 앱의 도구 정의만 전달한다. reasoning 내부 항목을 대화 기록으로 복원하지 않으므로 후속 호출에 내부 reasoning 연속성을 보장하지 않는다.

이 직접 연결은 공개 Codex 구현의 백엔드 규격을 따른다. 일반 OpenAI API의 안정적인 호환 계약과 같다고 보장하지 않으며 Codex 백엔드 변경 시 어댑터 수정이 필요할 수 있다. 실제 계정 접근·사용량 제한·모델 지원은 서버 응답으로 결정된다.

격리된 임시 인증 파일과 로컬 HTTP 서버로 우선순위·비밀정보 비노출·갱신·도구 변환·스트림 중단·취소를 검증한다. 실제 파일 기반 Codex 로그인으로 API 환경변수 없이 실행하여 timestamp 도구 호출·결과 전달·최종 OK 응답까지 2턴 완료를 확인했다. 설치된 릴리즈의 ACP 세션에서도 실제 Codex 계정으로 연속 2개 prompt가 end_turn과 응답 알림을 반환했다. 실제 OAuth 갱신은 모의 서버로 검증했으며, 모든 계정·모델·실제 에디터 연결을 검증한 것은 아니다.

공식 안내: [Codex 인증 캐시](https://learn.chatgpt.com/docs/auth#login-caching).
