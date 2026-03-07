FROM node:22-slim

WORKDIR /repl

COPY package.json ./
RUN npm install

COPY agent.mjs ./

RUN mkdir /data

ENV NODE_NO_WARNINGS=1

ENTRYPOINT ["node", "agent.mjs"]
