FROM node:22-alpine

WORKDIR /app/onchain

COPY onchain/package.json onchain/yarn.lock ./
RUN yarn install --frozen-lockfile --production=false

COPY onchain/ ./
COPY shared/chain/soldust.json ./target/idl/soldust.json
COPY shared/ /app/shared/

# shared/chain/*.js import @solana/web3.js and @coral-xyz/anchor, but Node
# resolves upward from /app/shared and never looks inside /app/onchain.
RUN ln -s /app/onchain/node_modules /app/node_modules

WORKDIR /app/onchain
ENV NODE_ENV=production

CMD ["yarn", "tsx", "scripts/crank.ts", "--interval", "4", "--auto-next-star"]
