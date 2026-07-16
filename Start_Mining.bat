@echo off
title Q-BTC Core Node
color 0A
echo ==================================================================
echo                  Q-BTC POST-QUANTUM NODE ENGINE
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
echo [STEP 3] Start CPU Mining! Type the command below and press Enter:
echo          auto_mine start mywallet
echo.
echo ==================================================================
echo [INFO] Bypassing Windows port limits (10013)...
echo [INFO] Launching node on port 19999...
echo.

.\quantum-btc.exe --port 19999

pause