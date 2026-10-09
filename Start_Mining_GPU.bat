@echo off
title Q-BTC Core Node (GPU)
color 0B

REM Always run from this script's own folder, no matter where it was launched from.
cd /d "%~dp0"

REM Use the CUDA backend. Set this to "hybrid" instead if you also want your
REM CPU cores mining alongside the GPU.
set QBTC_MINER_BACKEND=cuda

REM Leave the launch shape to the kernel defaults: one 1024-thread block per
REM streaming multiprocessor, which measured fastest. Clear any stale
REM overrides left in this console from a previous session.
set QBTC_CUDA_BLOCKS=
set QBTC_CUDA_THREADS=
set QBTC_CUDA_BATCH_SIZE=

echo ==================================================================
echo              Q-BTC POST-QUANTUM NODE ENGINE  (GPU)
echo ==================================================================
echo.
echo WELCOME MINER! To start earning Q-BTC, follow these 3 simple steps:
echo.
echo [STEP 1] Create your wallet. Type the command below and press Enter:
echo          wallet_gen mywallet
echo.
echo          *** IMPORTANT PASSWORD WARNING ***
echo          - It will ask for a password. Type it and press Enter.
echo          - (NOTE: You will NOT see anything while typing. This is normal!)
echo          - It will ask you to confirm. Type it again and press Enter.
echo.
echo [STEP 2] BACKUP YOUR SEED PHRASE!
echo          Write down the 12 words shown on screen on a piece of paper.
echo          (This is your only way to recover your funds.)
echo.
echo [STEP 3] Start mining! Type the command below and press Enter:
echo          auto_mine start mywallet
echo.
echo ------------------------------------------------------------------
echo [GPU] Once mining starts, look for these lines:
echo.
echo        Miner: CUDA device 0 ready: ...
echo        Miner: mining on 1 GPU(s): [0]
echo        [MINING] GPU0 progress: ... ^| 784.5 MH/s ^| ...
echo.
echo        The progress line repeats every 2 seconds.
echo.
echo        If you see "no CUDA device could be initialized" instead, the
echo        node is mining on your CPU. See docs\GPU_MINING.md.
echo ------------------------------------------------------------------
echo.
echo [INFO] Bypassing Windows port limits (10013)...
echo [INFO] Launching node on port 19999...
echo.

.\quantum-btc.exe --port 19999

pause
