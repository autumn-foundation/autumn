FROM maven:3.10-eclipse-temurin-25-alpine AS builder
WORKDIR /app
COPY pom.xml .
RUN mvn dependency:go-offline -q
COPY src ./src
RUN mvn package -DskipTests -q

FROM eclipse-temurin:25-jre-alpine
WORKDIR /app
COPY --from=builder /app/target/benchmark-0.1.0.jar app.jar

ENV SERVER_PORT=8080
EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=5s --start-period=60s \
  CMD wget -qO- http://localhost:8080/health || exit 1

# Half the 512 MiB limit goes to the heap. The rest holds metaspace, the code
# cache and up to 200 Tomcat thread stacks; at 75% the cgroup OOM killer
# stopped the JVM under 200 concurrent clients.
ENTRYPOINT ["java", "-XX:+UseContainerSupport", "-XX:MaxRAMPercentage=50.0", \
            "-Dspring.profiles.active=prod", "-jar", "app.jar"]
