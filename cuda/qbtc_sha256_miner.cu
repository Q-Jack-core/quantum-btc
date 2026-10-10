// Q-BTC CUDA SHA-256 PoW Miner
// Header: timestamp(8BE)|prev(32)|merkle(32)|commit(32)|nonce(8BE)|target(8BE) = 120 bytes
// SHA-256 of 120 bytes = 3 compression blocks:
//   Block1: bytes 0..63            -> fixed per job (midstate, host-computed)
//   Block2: bytes 64..119 + 0x80   -> W10 = nonce hi, W11 = nonce lo, rest fixed
//   Block3: zeros + bit length 960 -> fully constant message schedule
// PoW: sha256(header)[0..8] as u64 BE <= target
//
// Every kernel launch covers a nonce range that never crosses a 2^32
// boundary (the host splits it), so nonce-hi is a per-launch constant and
// the ONLY per-nonce input is W11 (nonce lo). Everything that does not depend
// on W11 is computed once per launch on the host and read from the kernel
// parameter bank (constant-bank operands, no registers, no loads):
//   - block2 rounds 0..10 entirely, and all of round 11 except "+ nonce_lo"
//   - message words W16,17,19,21,23 entirely, and the W11-independent partial
//     sums of every other expanded word
//   - K[i]+W[i] for block2's invariant words and all of block3's words
// The kernel is ALU-bound (funnel shifts are half-rate on sm_61), so fewer
// instructions per nonce is the only lever that moves MH/s.
//
// Launch shape: grid = SM count, 1024 threads/block -> exactly one block per
// SM (32 warps). Measured on the GTX 1060 3GB: 16-32 warps/SM ~785 MH/s,
// 48 warps ~700, 64 warps ~695 -- more resident warps is SLOWER here (the
// fully unrolled loop is ~16KB of SASS; warps at different PCs thrash the
// instruction cache). Sizing the grid to the SM count pins residency
// regardless of how many registers a given nvcc version allocates.
// __launch_bounds__(1024) keeps registers <= 64 so the launch can never
// fail with "too many resources requested".
#include <stdint.h>
#include <string.h>
#include <stdlib.h>

#define ROTR(x,n)(((x)>>(n))|((x)<<(32-(n))))
#define CH(e,f,g)(((e)&(f))^(~(e)&(g)))
#define MAJ(a,b,c)(((a)&(b))^((a)&(c))^((b)&(c)))
#define EP0(a)(ROTR(a,2)^ROTR(a,13)^ROTR(a,22))
#define EP1(e)(ROTR(e,6)^ROTR(e,11)^ROTR(e,25))
#define SIG0(x)(ROTR(x,7)^ROTR(x,18)^((x)>>3))
#define SIG1(x)(ROTR(x,17)^ROTR(x,19)^((x)>>10))

static const uint32_t HK[64]={0x428a2f98u,0x71374491u,0xb5c0fbcfu,0xe9b5dba5u,0x3956c25bu,0x59f111f1u,0x923f82a4u,0xab1c5ed5u,0xd807aa98u,0x12835b01u,0x243185beu,0x550c7dc3u,0x72be5d74u,0x80deb1feu,0x9bdc06a7u,0xc19bf174u,0xe49b69c1u,0xefbe4786u,0x0fc19dc6u,0x240ca1ccu,0x2de92c6fu,0x4a7484aau,0x5cb0a9dcu,0x76f988dau,0x983e5152u,0xa831c66du,0xb00327c8u,0xbf597fc7u,0xc6e00bf3u,0xd5a79147u,0x06ca6351u,0x14292967u,0x27b70a85u,0x2e1b2138u,0x4d2c6dfcu,0x53380d13u,0x650a7354u,0x766a0abbu,0x81c2c92eu,0x92722c85u,0xa2bfe8a1u,0xa81a664bu,0xc24b8b70u,0xc76c51a3u,0xd192e819u,0xd6990624u,0xf40e3585u,0x106aa070u,0x19a4c116u,0x1e376c08u,0x2748774cu,0x34b0bcb5u,0x391c0cb3u,0x4ed8aa4au,0x5b9cca4fu,0x682e6ff3u,0x748f82eeu,0x78a5636fu,0x84c87814u,0x8cc70208u,0x90befffau,0xa4506cebu,0xbef9a3f7u,0xc67178f2u};
static const uint32_t HINIT[8]={0x6a09e667u,0xbb67ae85u,0x3c6ef372u,0xa54ff53au,0x510e527fu,0x9b05688cu,0x1f83d9abu,0x5be0cd19u};

__constant__ uint32_t K[64];
__constant__ uint32_t KW3[64]; // K[i] + block3 W[i], identical for every header

// Bit i set <=> block2 word i depends on W11 (nonce lo). A constant, so
// every wvar(i) test below folds away after unrolling.
__host__ __device__ constexpr uint64_t wvar_mask(){
  uint64_t m=1ull<<11;
  for(int i=16;i<64;i++)
    if(((m>>(i-2))|(m>>(i-7))|(m>>(i-15))|(m>>(i-16)))&1ull) m|=1ull<<i;
  return m;
}
#define WVAR_MASK 0xffffffffff540800ULL
#define wvar(i) (((WVAR_MASK)>>(i))&1ull)
static_assert(wvar_mask()==WVAR_MASK,"WVAR_MASK out of sync");

struct QParams {
  uint32_t a11, e11;          // a,e after round 11 minus nonce_lo
  uint32_t b11,c11,d11,f11,g11,h11;
  uint32_t mid[2];            // block1 midstate words 0,1 (final block2 add)
  uint32_t mid_rest[6];       // midstate words 2..7
  uint32_t kw[64];            // K[i]+W[i] for invariant words i>=12
  uint32_t wc[64];            // invariant partial sum for W11-dependent words
  uint32_t nhi;
  uint32_t cmp_hi, cmp_lo;    // PoW target split
};

// Computes the PoW prefix (o0:o1). With EARLY_REJECT and a target whose high
// word is 0 (every difficulty since launch), o1 is final after round 62 of
// block3 -- b after round 63 is a after round 62 -- so a nonce with
// o1 > target_lo cannot pass: round 63 is skipped and o0 is set to
// 0xFFFFFFFF, which fails the o0 <= target_hi (= 0) comparison.
template<bool EARLY_REJECT>
__device__ __forceinline__ void pow_hash(const QParams& P, uint32_t nlo, uint32_t& o0, uint32_t& o1){
  uint32_t w[64];
  uint32_t a=P.a11+nlo, e=P.e11+nlo;
  uint32_t b=P.b11,c=P.c11,d=P.d11,f=P.f11,g=P.g11,h=P.h11;
  w[11]=nlo;
#pragma unroll
  for(int i=12;i<64;i++){
    uint32_t kw;
    if(wvar(i)){
      uint32_t x=P.wc[i];
      if(wvar(i-2))  x+=SIG1(w[i-2]);
      if(wvar(i-7))  x+=w[i-7];
      if(wvar(i-15)) x+=SIG0(w[i-15]);
      if(wvar(i-16)) x+=w[i-16];
      w[i]=x;
      kw=K[i]+x;
    } else {
      kw=P.kw[i];
    }
    uint32_t t1=h+EP1(e)+CH(e,f,g)+kw;
    uint32_t t2=EP0(a)+MAJ(a,b,c);
    h=g;g=f;f=e;e=d+t1;d=c;c=b;b=a;a=t1+t2;
  }
  uint32_t s0=P.mid[0]+a, s1=P.mid[1]+b;
  uint32_t s2=P.mid_rest[0]+c, s3=P.mid_rest[1]+d, s4=P.mid_rest[2]+e, s5=P.mid_rest[3]+f, s6=P.mid_rest[4]+g, s7=P.mid_rest[5]+h;
  a=s0;b=s1;c=s2;d=s3;e=s4;f=s5;g=s6;h=s7;
#pragma unroll
  for(int i=0;i<63;i++){
    uint32_t t1=h+EP1(e)+CH(e,f,g)+KW3[i];
    uint32_t t2=EP0(a)+MAJ(a,b,c);
    h=g;g=f;f=e;e=d+t1;d=c;c=b;b=a;a=t1+t2;
  }
  o1=s1+a;
  if(EARLY_REJECT && P.cmp_hi==0u && o1>P.cmp_lo){ o0=0xFFFFFFFFu; return; }
  uint32_t t1=h+EP1(e)+CH(e,f,g)+KW3[63];
  uint32_t t2=EP0(a)+MAJ(a,b,c);
  o0=s0+t1+t2;
}

#define STOP_CHECK_INTERVAL 1024u

__global__ void __launch_bounds__(1024) qbtc_mine_kernel(const QParams P,uint32_t slo,uint32_t len,uint32_t tot,uint64_t* found,volatile int* stopf){
  uint32_t tid=blockIdx.x*blockDim.x+threadIdx.x;
  if(tid>=len) return;
  uint32_t cnt=(uint32_t)(((uint64_t)(len-tid)+tot-1)/tot);
  uint32_t nlo=slo+tid;
  for(uint32_t it=0;it<cnt;it++,nlo+=tot){
    if((it&(STOP_CHECK_INTERVAL-1u))==0u){
      if(*stopf)return;
      if(*found!=0xFFFFFFFFFFFFFFFFULL)return;
    }
    uint32_t h0,h1;
    pow_hash<true>(P,nlo,h0,h1);
    if(h0<P.cmp_hi || (h0==P.cmp_hi && h1<=P.cmp_lo)){
      atomicCAS((unsigned long long*)found,0xFFFFFFFFFFFFFFFFULL,((unsigned long long)P.nhi<<32)|nlo);
      return;
    }
  }
}

#ifdef QBTC_DEBUG_EXPORTS
__global__ void qbtc_debug_kernel(const QParams P,uint32_t slo,uint32_t len,uint64_t* out){
  uint32_t i=blockIdx.x*blockDim.x+threadIdx.x;
  if(i>=len) return;
  uint32_t h0,h1; pow_hash<false>(P,slo+i,h0,h1);
  out[i]=((uint64_t)h0<<32)|h1;
}
#endif

static void host_transform(uint32_t s[8],const uint32_t win[16]){
  uint32_t w[64];int i;
  for(i=0;i<16;i++)w[i]=win[i];
  for(i=16;i<64;i++)w[i]=SIG1(w[i-2])+w[i-7]+SIG0(w[i-15])+w[i-16];
  uint32_t a=s[0],b=s[1],c=s[2],d=s[3],e=s[4],f=s[5],g=s[6],h=s[7];
  for(i=0;i<64;i++){uint32_t t1=h+EP1(e)+CH(e,f,g)+HK[i]+w[i];uint32_t t2=EP0(a)+MAJ(a,b,c);h=g;g=f;f=e;e=d+t1;d=c;c=b;b=a;a=t1+t2;}
  s[0]+=a;s[1]+=b;s[2]+=c;s[3]+=d;s[4]+=e;s[5]+=f;s[6]+=g;s[7]+=h;
}

// Per-launch host precompute for a fixed nonce-hi.
// hw = the 120-byte header as 30 big-endian words.
static void build_params(QParams& P,const uint32_t hw[30],uint32_t nhi,uint64_t tu64){
  int i;
  uint32_t mid[8];for(i=0;i<8;i++)mid[i]=HINIT[i];
  host_transform(mid,hw);
  uint32_t w[64];bool var[64];
  for(i=0;i<64;i++){w[i]=0;var[i]=wvar(i)!=0;}
  for(i=0;i<10;i++)w[i]=hw[16+i];
  w[10]=nhi; w[11]=0; w[12]=hw[28]; w[13]=hw[29];
  w[14]=0x80000000u; w[15]=0u;
  memset(P.wc,0,sizeof(P.wc)); memset(P.kw,0,sizeof(P.kw));
  for(i=16;i<64;i++){
    if(!var[i]){ w[i]=SIG1(w[i-2])+w[i-7]+SIG0(w[i-15])+w[i-16]; }
    else {
      uint32_t x=0;
      if(!var[i-2])  x+=SIG1(w[i-2]);
      if(!var[i-7])  x+=w[i-7];
      if(!var[i-15]) x+=SIG0(w[i-15]);
      if(!var[i-16]) x+=w[i-16];
      P.wc[i]=x;
    }
  }
  for(i=12;i<64;i++) if(!var[i]) P.kw[i]=HK[i]+w[i];
  uint32_t a=mid[0],b=mid[1],c=mid[2],d=mid[3],e=mid[4],f=mid[5],g=mid[6],h=mid[7];
  for(i=0;i<11;i++){uint32_t t1=h+EP1(e)+CH(e,f,g)+HK[i]+w[i];uint32_t t2=EP0(a)+MAJ(a,b,c);h=g;g=f;f=e;e=d+t1;d=c;c=b;b=a;a=t1+t2;}
  // Round 11 with W11 left out: t1 = T + nonce_lo.
  uint32_t T=h+EP1(e)+CH(e,f,g)+HK[11];
  uint32_t t2=EP0(a)+MAJ(a,b,c);
  P.a11=T+t2; P.e11=d+T;
  P.b11=a;P.c11=b;P.d11=c;P.f11=e;P.g11=f;P.h11=g;
  P.mid[0]=mid[0];P.mid[1]=mid[1];
  for(i=0;i<6;i++)P.mid_rest[i]=mid[2+i];
  P.nhi=nhi;
  P.cmp_hi=(uint32_t)(tu64>>32); P.cmp_lo=(uint32_t)tu64;
}

// Per-device state. cudaSetDevice() is per-host-thread, and a device's
// primary context is shared process-wide, so one worker thread per GPU can
// each drive its own device as long as every entry point selects the device
// and touches only that device's buffers.
#define QBTC_MAX_DEV 8
static uint64_t* g_dfound[QBTC_MAX_DEV] = {0};
static int* g_dstop[QBTC_MAX_DEV] = {0};
// Filled in per device by qbtc_cuda_init_device() from the device's real
// multiprocessor count; only ever read after that has succeeded.
static int g_sm_count[QBTC_MAX_DEV] = {0};
#ifdef QBTC_DEBUG_EXPORTS
static uint64_t* g_ddbg = 0;
#endif

extern "C" {
int qbtc_cuda_mine_on(int dev,const uint8_t* h120,uint64_t tu64,uint64_t start,uint64_t bsz,uint64_t* out,volatile int* sf){
  if(dev<0||dev>=QBTC_MAX_DEV) return -6;
  if(!g_dfound[dev] || !g_dstop[dev]) return -1;
  if(cudaSetDevice(dev)!=cudaSuccess) return -7;
  uint32_t hw[30];
  for(int i=0;i<30;i++){const uint8_t*p=h120+i*4;hw[i]=((uint32_t)p[0]<<24)|((uint32_t)p[1]<<16)|((uint32_t)p[2]<<8)|(uint32_t)p[3];}
  uint64_t sen=0xFFFFFFFFFFFFFFFFULL;int zero=0;
  if(cudaMemcpy(g_dfound[dev],&sen,sizeof(uint64_t),cudaMemcpyHostToDevice)!=cudaSuccess)return -3;
  if(cudaMemcpy(g_dstop[dev],&zero,sizeof(int),cudaMemcpyHostToDevice)!=cudaSuccess)return -4;
  const char*eb=getenv("QBTC_CUDA_BLOCKS");const char*et=getenv("QBTC_CUDA_THREADS");
  int nb=eb?atoi(eb):g_sm_count[dev];int nt=et?atoi(et):1024;
  if(nb<1)nb=1; if(nt<1||nt>1024)nt=1024;
  uint32_t tot=(uint32_t)nb*(uint32_t)nt;
  uint64_t cur=start, remaining=bsz;
  while(remaining>0){
    uint32_t nhi=(uint32_t)(cur>>32), slo=(uint32_t)cur;
    uint64_t room=0x100000000ULL-(uint64_t)slo;
    uint64_t len=remaining<room?remaining:room;
    if(len>0x80000000ULL) len=0x80000000ULL;
    QParams P;
    build_params(P,hw,nhi,tu64);
    qbtc_mine_kernel<<<nb,nt>>>(P,slo,(uint32_t)len,tot,g_dfound[dev],g_dstop[dev]);
    if(cudaDeviceSynchronize()!=cudaSuccess)return -1;
    uint64_t res=0;
    if(cudaMemcpy(&res,g_dfound[dev],sizeof(uint64_t),cudaMemcpyDeviceToHost)!=cudaSuccess)return -5;
    if(res!=0xFFFFFFFFFFFFFFFFULL){*out=res;return 0;}
    cur+=len; remaining-=len;
    if(sf&&*sf)break;
  }
  return 1;
}
int qbtc_cuda_device_count(int*n){
  if(!n)return -1;
  cudaError_t e=cudaGetDeviceCount(n);
  if(e!=cudaSuccess||*n<=0){*n=0;return -1;}
  if(*n>QBTC_MAX_DEV)*n=QBTC_MAX_DEV;
  return 0;}
// Prepares exactly one device: selects it, uploads the __constant__ tables
// into that device's context (constant memory is per-device, so this has to
// run once per GPU) and allocates that device's found/stop buffers.
int qbtc_cuda_init_device(int dev){
  int count=0;
  if(cudaGetDeviceCount(&count)!=cudaSuccess||count<=0)return -1;
  if(dev<0||dev>=count||dev>=QBTC_MAX_DEV)return -6;
  if(cudaSetDevice(dev)!=cudaSuccess)return -7;
  cudaSetDeviceFlags(cudaDeviceScheduleBlockingSync);
  int sms=0;
  if(cudaDeviceGetAttribute(&sms,cudaDevAttrMultiProcessorCount,dev)==cudaSuccess&&sms>0)g_sm_count[dev]=sms;
  uint32_t w[64],kw3[64];int i;
  for(i=0;i<15;i++)w[i]=0u;
  w[15]=0x000003c0u;
  for(i=16;i<64;i++)w[i]=SIG1(w[i-2])+w[i-7]+SIG0(w[i-15])+w[i-16];
  for(i=0;i<64;i++)kw3[i]=HK[i]+w[i];
  if(cudaMemcpyToSymbol(K,HK,sizeof(HK))!=cudaSuccess)return -1;
  if(cudaMemcpyToSymbol(KW3,kw3,sizeof(kw3))!=cudaSuccess)return -1;
  if(g_dfound[dev]){cudaFree(g_dfound[dev]);g_dfound[dev]=0;}
  if(g_dstop[dev]){cudaFree(g_dstop[dev]);g_dstop[dev]=0;}
  if(cudaMalloc(&g_dfound[dev],sizeof(uint64_t))!=cudaSuccess)return -1;
  if(cudaMalloc(&g_dstop[dev],sizeof(int))!=cudaSuccess){cudaFree(g_dfound[dev]);g_dfound[dev]=0;return -1;}
  return 0;}
const char* qbtc_cuda_device_name(int id){static char nm[256];cudaDeviceProp p;if(cudaGetDeviceProperties(&p,id)!=cudaSuccess)return "Unknown";strncpy(nm,p.name,255);nm[255]=0;return nm;}
void qbtc_cuda_shutdown(void){
  for(int d=0;d<QBTC_MAX_DEV;d++){
    if(!g_dfound[d]&&!g_dstop[d])continue;
    if(cudaSetDevice(d)!=cudaSuccess)continue;
    if(g_dfound[d]){cudaFree(g_dfound[d]);g_dfound[d]=0;}
    if(g_dstop[d]){cudaFree(g_dstop[d]);g_dstop[d]=0;}
    cudaDeviceReset();
  }}

#ifdef QBTC_DEBUG_EXPORTS
// Writes the PoW u64 (hash bytes 0..7, BE) for nonces start..start+n-1.
int qbtc_cuda_debug_hashes(const uint8_t* h120,uint64_t start,uint64_t n,uint64_t* out){
  uint32_t hw[30];
  for(int i=0;i<30;i++){const uint8_t*p=h120+i*4;hw[i]=((uint32_t)p[0]<<24)|((uint32_t)p[1]<<16)|((uint32_t)p[2]<<8)|(uint32_t)p[3];}
  uint64_t cur=start,done=0;
  if(g_ddbg){cudaFree(g_ddbg);g_ddbg=0;}
  if(cudaMalloc(&g_ddbg,n*sizeof(uint64_t))!=cudaSuccess)return -1;
  while(done<n){
    uint32_t nhi=(uint32_t)(cur>>32), slo=(uint32_t)cur;
    uint64_t room=0x100000000ULL-(uint64_t)slo;
    uint64_t len=(n-done)<room?(n-done):room;
    QParams P;
    build_params(P,hw,nhi,0);
    qbtc_debug_kernel<<<(unsigned)((len+255)/256),256>>>(P,slo,(uint32_t)len,g_ddbg+done);
    if(cudaDeviceSynchronize()!=cudaSuccess)return -1;
    cur+=len; done+=len;
  }
  if(cudaMemcpy(out,g_ddbg,n*sizeof(uint64_t),cudaMemcpyDeviceToHost)!=cudaSuccess)return -1;
  return 0;
}
#endif
}
