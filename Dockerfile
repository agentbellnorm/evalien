FROM node:22-slim

WORKDIR /repl

COPY package.json ./
RUN npm install

COPY agent.mjs ./

# No host env vars leak in — API key must be passed explicitly at runtime
ENV NODE_NO_WARNINGS=1

ENTRYPOINT ["node", "agent.mjs"]
