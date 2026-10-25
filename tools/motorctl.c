/* Exact AK3918EN088 stock-driver CLI. No homing or position persistence. */
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <fcntl.h>
#include <signal.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/file.h>
#include <dirent.h>
#include <time.h>
static volatile sig_atomic_t interrupted;
static void handler(int sig) { (void)sig; interrupted=1; }
static long ms(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC,&t); return t.tv_sec*1000L+t.tv_nsec/1000000; }
static int number(const char *s,int lo,int hi) { char *end; long n=strtol(s,&end,10); if(!*s||*end||n<lo||n>hi) return -1; return (int)n; }
static int other_owner(void) {
 DIR *p=opendir("/proc"); struct dirent *e; if(!p) return 1;
 while((e=readdir(p))) { int pid=atoi(e->d_name); if(pid<=0||pid==getpid()) continue;
 char path[256]; snprintf(path,sizeof path,"/proc/%d/fd",pid); DIR *d=opendir(path); if(!d) continue; struct dirent *f;
 while((f=readdir(d))) { char link[512],target[256]; snprintf(link,sizeof link,"%s/%s",path,f->d_name); ssize_t n=readlink(link,target,sizeof target-1); if(n<0) continue; target[n]=0;
 if(!strcmp(target,"/dev/ak-motor0")||!strcmp(target,"/dev/ak-motor1")) { fprintf(stderr,"Refusing: PID %d owns %s\n",pid,target); closedir(d); closedir(p); return 1; }
 } closedir(d);
 } closedir(p); return 0;
}
static int wait_stop(int fd,int timeout) {
 long deadline=ms()+timeout;
 while(ms()<deadline) { struct pollfd p={fd,POLLIN,0}; int r=poll(&p,1,50); if(r<0&&errno==EINTR) continue; if(r<0) return -1;
 if(r>0&&(p.revents&POLLIN)) { uint32_t event[3]={0}; /* Stock read copies 12 bytes but returns ZERO. */
 ssize_t n=read(fd,event,sizeof event); if(n<0) return -1; if(event[1]&4) { printf("Stop event: flags=%u remaining=%u\n",event[1],event[2]); return 0; }
 } if(interrupted) return -1;
 } return -1;
}
int main(int argc,char **argv) {
 if(argc==1) { puts("Idle: no motor opened or moved. Usage: motorctl AXIS(0|1) DIR(0|1) STEPS(1..32) SPEED(1..10)"); return 0; }
 if(argc!=5) return 2;
 int axis=number(argv[1],0,1),dir=number(argv[2],0,1),steps=number(argv[3],1,32),speed=number(argv[4],1,10);
 if(axis<0||dir<0||steps<0||speed<0) { fputs("Invalid argument; nothing opened.\n",stderr); return 2; }
 int lock=open("/tmp/motorctl.lock",O_CREAT|O_RDWR,0600); if(lock<0||flock(lock,LOCK_EX|LOCK_NB)) { perror("lock"); return 1; }
 if(other_owner()) return 1;
 signal(SIGINT,handler); signal(SIGTERM,handler); signal(SIGHUP,handler);
 char dev[32]; snprintf(dev,sizeof dev,"/dev/ak-motor%d",axis); int fd=open(dev,O_RDWR|O_NONBLOCK); if(fd<0) { perror("open motor"); return 1; }
 int zero=0,rc=1;
 if(ioctl(fd,0x40046d0b,&speed)<0) { perror("speed"); goto done; }
 if(interrupted) goto done;
 printf("Axis %d direction %d: %d phase steps, speed %d\n",axis,dir,steps,speed); fflush(stdout);
 if(ioctl(fd,dir?0x40046d0e:0x40046d0d,&steps)<0) { perror("move"); goto done; }
 /* Slowest setting: 5 phase updates/sec; include completion callback. */
 if(wait_stop(fd,8000)==0) { rc=0; goto done; }
 fputs("No normal completion (or interrupted); requesting stop.\n",stderr);
 ioctl(fd,0x40046d10,&zero); interrupted=0;
 if(wait_stop(fd,1500)<0) fputs("STOP NOT CONFIRMED: remove motor power if still moving.\n",stderr);
 done: ioctl(fd,0x40046d10,&zero); close(fd); close(lock); return rc;
}
