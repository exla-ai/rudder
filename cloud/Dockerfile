FROM node:22-slim

WORKDIR /app

RUN apt-get update \
  && apt-get install -y --no-install-recommends git openssh-client ca-certificates curl \
  && rm -rf /var/lib/apt/lists/*

COPY package.json package-lock.json* ./
RUN npm install

COPY tsconfig.json ./
COPY src ./src
COPY app ./app
RUN npm run build

ENV NODE_ENV=production
ENV PORT=3000
ENV RUDDER_CLOUD_DATA_DIR=/data

EXPOSE 3000

CMD ["npm", "start"]
