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


