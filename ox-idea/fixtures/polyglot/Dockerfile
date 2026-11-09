FROM golang:1.22-alpine AS build
WORKDIR /src
COPY api/ ./
RUN go build -o /out/api .

FROM alpine:3.20
# TODO: run as a non-root user
COPY --from=build /out/api /usr/local/bin/api
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/api"]
