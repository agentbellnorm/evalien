FROM node:22-slim

WORKDIR /repl

COPY package.json ./
RUN npm install

COPY agent.mts util.mts ./

RUN mkdir /data

ENV NODE_NO_WARNINGS=1

ENTRYPOINT ["node", "--experimental-strip-types", "agent.mts"]
