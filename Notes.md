<!-- Command to run VLLM -->
docker run --rm \
    --privileged=true \
    --shm-size=1g \
    -p 8000:8000 \
    -e VLLM_CPU_OMP_THREADS_BIND=0-3 \
    -e VLLM_CPU_KVCACHE_SPACE=1 \
    -v /Volumes/extdisk1/Sandeep-code/model-cache/hf:/models \
    vllm-cpu-env \
    --model /models/llama-3.2-1b-instruct \
    --dtype bfloat16 \
    --max-model-len 4096 \
    --max-num-seqs 1


# Click house
docker run --rm -d -p 18123:8123 -p19000:9000 -e CLICKHOUSE_PASSWORD=changeme --name clickhouse-server-tupleap --ulimit nofile=262144:262144 clickhouse/clickhouse-server
CREATE DATABASE IF NOT EXISTS tensorzero;

INSERT INTO tensorzero.AUTHCode (auth_code, tenant_id, username, created_at, is_active, usage_count, created_by, expires_at) VALUES ('tupleap_demo001', 'demo001', 'xxx', '2025-07-24 13:00:00.000', 1, 0, 'admin', NULL);


# clickhouse ui
docker run -d -p 8999:80 spoonest/clickhouse-tabix-web-client

docker run --name ch-ui --rm -p 5521:5521 \
  -e VITE_CLICKHOUSE_URL=http://localhost:18123 \
  -e VITE_CLICKHOUSE_USER=default \
  -e VITE_CLICKHOUSE_PASS=changeme \
  -e VITE_CLICKHOUSE_REQUEST_TIMEOUT=30000 \
  ghcr.io/caioricciuti/ch-ui:latest





# build steps

docker buildx create \
  --name container-builder \
  --driver docker-container \
  --use \
  --bootstrap



DOCKER_BUILDKIT=1 docker buildx build \
  --platform linux/amd64 \
  -t tensorzero/gateway:latest \
  -f gateway/Dockerfile \
  --load .


# Testing steps
curl -X POST http://localhost:3000/inference \
  -H "Content-Type: application/json" \
  -H "TUPLEAP_AUTHCODE: tupleap_demo001" \
  -d '{
    "function_name": "chat_tupleap_generic",
    "input": {
      "messages": [
        {
          "role": "user",
          "content": "What is the capital of Japan?"
        }
      ]
    }
  }' | jq .