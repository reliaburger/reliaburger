#define _POSIX_C_SOURCE 200809L
#include <pthread.h>
#include <spawn.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
extern char **environ;
static atomic_ulong next_job, completed, failed;
static unsigned long count;
static const char *mode, *executable;
static uint64_t task(unsigned long index) {
  uint64_t value = index ^ UINT64_C(14695981039346656037);
  for (unsigned j = 0; j < 64; j++) value = (value ^ j) * UINT64_C(1099511628211);
  return value;
}
static void *worker(void *unused) {
  (void)unused;
  uint64_t checksum = 0;
  for (;;) {
    unsigned long index = atomic_fetch_add(&next_job, 1);
    if (index >= count) break;
    if (!strcmp(mode, "spawn")) {
      char number[32]; snprintf(number, sizeof number, "%lu", index);
      char *args[] = {(char *)executable, "job", number, NULL};
      pid_t pid; int status;
      int error = posix_spawn(&pid, executable, NULL, NULL, args, environ);
      if (error || waitpid(pid, &status, 0) != pid || !WIFEXITED(status) || WEXITSTATUS(status)) {
        atomic_fetch_add(&failed, 1); continue;
      }
    } else checksum ^= task(index);
    atomic_fetch_add(&completed, 1);
  }
  return (void *)(uintptr_t)checksum;
}
int main(int argc, char **argv) {
  if (argc == 3 && !strcmp(argv[1], "job")) {
    volatile uint64_t result = task(strtoul(argv[2], NULL, 10));
    (void)result; return 0;
  }
  if (argc != 4 || (strcmp(argv[1], "spawn") && strcmp(argv[1], "pooled"))) return 64;
  mode=argv[1]; count=strtoul(argv[2],NULL,10); unsigned slots=strtoul(argv[3],NULL,10);
  if (!count || !slots || slots>64) return 64;
  executable=argv[0]; pthread_t workers[64]; struct timespec start,end;
  clock_gettime(CLOCK_MONOTONIC,&start);
  for(unsigned i=0;i<slots;i++) if(pthread_create(&workers[i],NULL,worker,NULL)) return 1;
  uint64_t checksum=0;
  for(unsigned i=0;i<slots;i++) {void *result; pthread_join(workers[i],&result); checksum^=(uint64_t)(uintptr_t)result;}
  clock_gettime(CLOCK_MONOTONIC,&end);
  double seconds=(end.tv_sec-start.tv_sec)+(end.tv_nsec-start.tv_nsec)/1e9;
  printf("{\"mode\":\"%s\",\"tasks\":%lu,\"completed\":%lu,\"failed\":%lu,\"slots\":%u,\"seconds\":%.6f,\"rate\":%.3f,\"checksum\":%llu}\n",mode,count,atomic_load(&completed),atomic_load(&failed),slots,seconds,atomic_load(&completed)/seconds,(unsigned long long)checksum);
  return atomic_load(&failed) || atomic_load(&completed)!=count;
}
