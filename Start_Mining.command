#!/bin/bash

cd "$(dirname "$0")" || exit
clear


GREEN='\033[1;32m'
YELLOW='\033[1;33m'
CYAN='\033[1;36m'
RED='\033[1;31m'
NC='\033[0m' 

echo -e "${CYAN}==================================================================${NC}"
echo -e "${CYAN}                 Q-BTC POST-QUANTUM NODE ENGINE                   ${NC}"
echo -e "${CYAN}==================================================================${NC}\n"

echo -e "${GREEN}WELCOME MINER! To start earning Q-BTC, read these 3 simple steps:${NC}\n"

echo -e "${YELLOW}[STEP 1] Create your wallet.${NC} Once the node starts, type the command below and press Enter:"
echo -e "         ${CYAN}wallet_gen mywallet${NC}\n"
echo -e "         ${RED}*** IMPORTANT PASSWORD WARNING ***${NC}"
echo -e "         - It will ask for a password. Type it and press Enter."
echo -e "         - (NOTE: You will NOT see anything while typing. This is normal!)"
echo -e "         - It will ask you to confirm. Type it again and press Enter.\n"

echo -e "${YELLOW}[STEP 2] BACKUP YOUR SEED PHRASE!${NC}"
echo -e "         Write down the 12 words shown on screen on a piece of paper."
echo -e "         (This is your only way to recover your funds.)\n"

echo -e "${YELLOW}[STEP 3] Start CPU Mining!${NC} Type the command below and press Enter:"
echo -e "         ${CYAN}auto_mine start mywallet${NC}\n"

echo -e "${CYAN}==================================================================${NC}\n"

# Verify if the core engine exists
if [ ! -f "./quantum-btc" ]; then
    echo -e "${RED}[ERROR] Core engine 'quantum-btc' not found in this folder.${NC}"
    read -p "Press [Enter] to exit..."
    exit 1
fi

chmod +x "./quantum-btc"

echo -e "${RED}>>> READ THE STEPS ABOVE CAREFULLY <<<${NC}"
read -p "Press [Enter] when you are ready to launch the node..."


echo -e "\n${GREEN}[INFO] Launching post-quantum node...${NC}\n"
./quantum-btc

echo ""
read -p "Node has safely powered down. Press [Enter] to close this window..."

osascript -e 'tell application "Terminal" to close front window' > /dev/null 2>&1
exit 0